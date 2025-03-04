# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from pathlib import Path

from materialize import spawn
from materialize.mzcompose import Composition
from materialize.mzcompose.services import (
    Coordd,
    Dataflowd,
    Kafka,
    SchemaRegistry,
    Testdrive,
    Zookeeper,
)

SERVICES = [
    Zookeeper(),
    Kafka(),
    SchemaRegistry(),
    Dataflowd(
        name="dataflowd_1",
        options="--workers 2 --processes 2 --process 0 dataflowd_1:2101 dataflowd_2:2101",
        hostname="dataflowd_1",
        ports=[6876, 2101],
    ),
    Dataflowd(
        name="dataflowd_2",
        options="--workers 2 --processes 2 --process 1 dataflowd_1:2101 dataflowd_2:2101",
        hostname="dataflowd_2",
        ports=[6876, 2101],
    ),
    Coordd(
        name="materialized",
        options="--workers 4 dataflowd_1:6876 dataflowd_2:6876",
    ),
    Testdrive(
        volumes=[
            "mzdata:/share/mzdata",
            "tmp:/share/tmp",
            ".:/workdir/smoke",
            "../testdrive:/workdir/testdrive",
        ],
    ),
]


def workflow_cluster_smoke(c: Composition) -> None:
    test_cluster(c, "smoke/*.td")


def workflow_cluster_testdrive(c: Composition) -> None:
    c.start_and_wait_for_tcp(services=["zookeeper", "kafka", "schema-registry"])
    # Skip tests that use features that are not supported yet.
    files = spawn.capture(
        [
            "sh",
            "-c",
            "grep -rLE 'mz_catalog|mz_kafka_|mz_records_|mz_metrics' testdrive/*.td",
        ],
        cwd=Path(__file__).parent.parent,
    ).split()
    test_cluster(c, *files)


def test_cluster(c: Composition, *glob: str) -> None:
    c.up("dataflowd_1", "dataflowd_2")
    c.up("materialized")
    c.wait_for_materialized()
    c.run("testdrive-svc", *glob)
