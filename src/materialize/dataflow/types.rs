// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use serde::{Deserialize, Serialize};
use url::Url;

use super::func::{AggregateFunc, BinaryFunc, UnaryFunc, VariadicFunc};
use crate::repr::{ColumnType, Datum, RelationType};

/// System-wide update type.
pub type Diff = isize;

/// A named stream of data.
#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Dataflow {
    Source(Source),
    Sink(Sink),
    View(View),
}

impl Dataflow {
    /// Reports the name of this dataflow.
    pub fn name(&self) -> &str {
        match self {
            Dataflow::Source(src) => &src.name,
            Dataflow::Sink(sink) => &sink.name,
            Dataflow::View(view) => &view.name,
        }
    }

    /// Reports the type of the datums produced by this dataflow.
    pub fn typ(&self) -> &RelationType {
        match self {
            Dataflow::Source(src) => &src.typ,
            Dataflow::Sink(sink) => &sink.from.1,
            Dataflow::View(view) => &view.typ,
        }
    }

    /// Collects the names of the dataflows that this dataflow depends upon.
    pub fn uses(&self) -> Vec<&str> {
        let mut out = Vec::new();
        match self {
            Dataflow::Source(_) => (),
            Dataflow::Sink(sink) => out.push(sink.from.0.as_str()),
            Dataflow::View(view) => view.relation_expr.uses_inner(&mut out),
        }
        out
    }
}

impl metastore::Dataflow for Dataflow {}

/// A data source materializes data. It typically represents an external source
/// of data, like a topic from Apache Kafka.
#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub connector: SourceConnector,
    pub typ: RelationType,
}

#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Sink {
    pub name: String,
    pub from: (String, RelationType),
    pub connector: SinkConnector,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SourceConnector {
    Kafka(KafkaSourceConnector),
    Local(LocalSourceConnector),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KafkaSourceConnector {
    pub addr: std::net::SocketAddr,
    pub topic: String,
    pub raw_schema: String,
    #[serde(with = "url_serde")]
    pub schema_registry_url: Option<Url>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LocalSourceConnector {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SinkConnector {
    Kafka(KafkaSinkConnector),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KafkaSinkConnector {
    pub addr: std::net::SocketAddr,
    pub topic: String,
    pub schema_id: i32,
}

/// A view transforms one dataflow into another.
#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub name: String,
    pub relation_expr: RelationExpr,
    pub typ: RelationType,
}

#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub enum RelationExpr {
    /// Always return the same value
    Constant {
        rows: Vec<Vec<Datum>>,
        typ: RelationType,
    },
    /// Get an existing dataflow
    Get { name: String, typ: RelationType },
    /// Introduce a temporary dataflow
    Let {
        name: String,
        value: Box<RelationExpr>,
        body: Box<RelationExpr>,
    },
    /// Project out some columns from a dataflow
    Project {
        input: Box<RelationExpr>,
        outputs: Vec<usize>,
    },
    /// Append new columns to a dataflow
    Map {
        input: Box<RelationExpr>,
        // these are appended to output in addition to all the columns of input
        scalars: Vec<(ScalarExpr, ColumnType)>,
    },
    /// Keep rows from a dataflow where all the predicates are true
    Filter {
        input: Box<RelationExpr>,
        predicates: Vec<ScalarExpr>,
    },
    /// Join several dataflows together at once
    Join {
        inputs: Vec<RelationExpr>,
        // each HashSet is an equivalence class of (input_index, column_index)
        variables: Vec<Vec<(usize, usize)>>,
    },
    /// Group a dataflow by some columns and aggregate over each group
    Reduce {
        input: Box<RelationExpr>,
        group_key: Vec<usize>,
        // these are appended to output in addition to all the columns of input that are in group_key
        aggregates: Vec<(AggregateExpr, ColumnType)>,
    },
    /// Groups and orders within each group, limiting output.
    TopK {
        input: Box<RelationExpr>,
        group_key: Vec<usize>,
        order_key: Vec<usize>,
        limit: usize,
    },
    /// If the input is empty, return a default row
    // Used only for some SQL aggregate edge cases
    OrDefault {
        input: Box<RelationExpr>,
        default: Vec<Datum>,
    },
    /// Return a dataflow where the row counts are negated
    Negate { input: Box<RelationExpr> },
    /// Return a dataflow where the row counts are all set to 1
    Distinct { input: Box<RelationExpr> },
    /// Return the union of two dataflows
    Union {
        left: Box<RelationExpr>,
        right: Box<RelationExpr>,
    },
    // TODO Lookup/Arrange
}

impl RelationExpr {
    pub fn typ(&self) -> RelationType {
        match self {
            RelationExpr::Constant { rows, typ } => {
                for row in rows {
                    for (datum, column_typ) in row.iter().zip(typ.column_types.iter()) {
                        assert!(datum.is_instance_of(column_typ));
                    }
                }
                typ.clone()
            }
            RelationExpr::Get { typ, .. } => typ.clone(),
            RelationExpr::Let { body, .. } => body.typ(),
            RelationExpr::Project { input, outputs } => {
                let input_typ = input.typ();
                RelationType {
                    column_types: outputs
                        .iter()
                        .map(|&i| input_typ.column_types[i].clone())
                        .collect(),
                }
            }
            RelationExpr::Map { input, scalars } => {
                let mut typ = input.typ();
                for (_, column_typ) in scalars {
                    typ.column_types.push(column_typ.clone());
                }
                typ
            }
            RelationExpr::Filter { input, .. } => input.typ(),
            RelationExpr::Join { inputs, .. } => {
                let mut column_types = vec![];
                for input in inputs {
                    column_types.append(&mut input.typ().column_types);
                }
                RelationType { column_types }
            }
            RelationExpr::Reduce {
                input,
                group_key,
                aggregates,
            } => {
                let input_typ = input.typ();
                let mut column_types = group_key
                    .iter()
                    .map(|&i| input_typ.column_types[i].clone())
                    .collect::<Vec<_>>();
                for (_, column_typ) in aggregates {
                    column_types.push(column_typ.clone());
                }
                RelationType { column_types }
            }
            RelationExpr::TopK { input, .. } => input.typ(),
            RelationExpr::OrDefault { input, default } => {
                let typ = input.typ();
                for (column_typ, datum) in typ.column_types.iter().zip(default.iter()) {
                    assert!(datum.scalar_type().is_instance_of(column_typ));
                }
                typ
            }
            RelationExpr::Negate { input } => input.typ(),
            RelationExpr::Distinct { input } => input.typ(),
            RelationExpr::Union { left, right } => {
                let left_typ = left.typ();
                let right_typ = right.typ();
                assert_eq!(left_typ.column_types.len(), right_typ.column_types.len());
                RelationType {
                    column_types: left_typ
                        .column_types
                        .iter()
                        .zip(right_typ.column_types.iter())
                        .map(|(l, r)| l.union(r))
                        .collect(),
                }
            }
        }
    }

    pub fn arity(&self) -> usize {
        self.typ().column_types.len()
    }
}

impl RelationExpr {
    pub fn project(self, outputs: Vec<usize>) -> Self {
        RelationExpr::Project {
            input: Box::new(self),
            outputs,
        }
    }
    pub fn map(self, scalars: Vec<(ScalarExpr, ColumnType)>) -> Self {
        RelationExpr::Map {
            input: Box::new(self),
            scalars,
        }
    }
    pub fn filter(self, predicates: Vec<ScalarExpr>) -> Self {
        RelationExpr::Filter {
            input: Box::new(self),
            predicates,
        }
    }
    pub fn reduce(
        self,
        group_key: Vec<usize>,
        aggregates: Vec<(AggregateExpr, ColumnType)>,
    ) -> Self {
        RelationExpr::Reduce {
            input: Box::new(self),
            group_key,
            aggregates,
        }
    }
    pub fn or_default(self, default: Vec<Datum>) -> Self {
        RelationExpr::OrDefault {
            input: Box::new(self),
            default,
        }
    }
    pub fn negate(self) -> Self {
        RelationExpr::Negate {
            input: Box::new(self),
        }
    }
    pub fn distinct(self) -> Self {
        RelationExpr::Distinct {
            input: Box::new(self),
        }
    }
    pub fn union(self, other: Self) -> Self {
        RelationExpr::Union {
            left: Box::new(self),
            right: Box::new(other),
        }
    }
}

impl RelationExpr {
    /// Collects the names of the dataflows that this relation_expr depends upon.
    #[allow(clippy::unneeded_field_pattern)]
    fn uses_inner<'a, 'b>(&'a self, out: &'b mut Vec<&'a str>) {
        self.visit(|e| {
            if let RelationExpr::Get { name, .. } = e {
                out.push(&name);
            }
        });
    }
}

#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct AggregateExpr {
    pub func: AggregateFunc,
    pub expr: ScalarExpr,
    pub distinct: bool,
}

#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub enum ScalarExpr {
    /// A column of the input row
    Column(usize),
    /// A literal value.
    Literal(Datum),
    /// A function call that takes one expression as an argument.
    CallUnary {
        func: UnaryFunc,
        expr: Box<ScalarExpr>,
    },
    /// A function call that takes two expressions as arguments.
    CallBinary {
        func: BinaryFunc,
        expr1: Box<ScalarExpr>,
        expr2: Box<ScalarExpr>,
    },
    /// A function call that takes an arbitrary number of arguments.
    CallVariadic {
        func: VariadicFunc,
        exprs: Vec<ScalarExpr>,
    },
    If {
        cond: Box<ScalarExpr>,
        then: Box<ScalarExpr>,
        els: Box<ScalarExpr>,
    },
}

impl ScalarExpr {
    pub fn columns(is: &[usize]) -> Vec<ScalarExpr> {
        is.iter().map(|i| ScalarExpr::Column(*i)).collect()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use std::error::Error;

    use super::*;
    use crate::repr::{ColumnType, ScalarType};

    /// Verify that a basic relation_expr serializes and deserializes to JSON sensibly.
    #[test]
    fn test_roundtrip() -> Result<(), Box<dyn Error>> {
        let dataflow = Dataflow::View(View {
            name: "report".into(),
            relation_expr: RelationExpr::Project {
                outputs: vec![ScalarExpr::Column(1), ScalarExpr::Column(2)],
                input: Box::new(RelationExpr::Join {
                    left_key: vec![ScalarExpr::Column(0)],
                    right_key: vec![ScalarExpr::Column(0)],
                    left: Box::new(RelationExpr::Source("orders".into())),
                    right: Box::new(RelationExpr::Distinct(Box::new(RelationExpr::UnionAll(
                        vec![
                            RelationExpr::Source("customers2018".into()),
                            RelationExpr::Source("customers2019".into()),
                        ],
                    )))),
                    include_left_outer: None,
                    include_right_outer: None,
                }),
            },
            typ: RelationType {
                column_types: vec![
                    ColumnType {
                        name: Some("name".into()),
                        nullable: false,
                        scalar_type: ScalarType::String,
                    },
                    ColumnType {
                        name: Some("quantity".into()),
                        nullable: false,
                        scalar_type: ScalarType::Int32,
                    },
                ],
            },
        });

        let decoded: Dataflow = serde_json::from_str(&serde_json::to_string_pretty(&dataflow)?)?;
        assert_eq!(decoded, dataflow);

        Ok(())
    }
}
