-- Copyright Materialize, Inc. and contributors. All rights reserved.
--
-- Use of this software is governed by the Business Source License
-- included in the LICENSE file at the root of this repository.
--
-- As of the Change Date specified in that file, in accordance with
-- the Business Source License, use of this software will be governed
-- by the Apache License, Version 2.0.

-- Sets up a CockroachDB cluster for use by Materialize.

-- See: https://github.com/cockroachdb/cockroach/issues/93892
-- See: https://github.com/MaterializeInc/materialize/issues/16726
-- TODO: remove this workaround before upgrading to CockroachDB 22.2 in
-- production.
SET CLUSTER SETTING sql.stats.forecasts.enabled = false;

CREATE SCHEMA consensus;
CREATE SCHEMA adapter;
CREATE SCHEMA storage;
