# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

"""The implementation of the mzcompose system for Docker compositions.

For an overview of what mzcompose is and why it exists, see the [user-facing
documentation][user-docs].

[user-docs]: https://github.com/MaterializeInc/materialize/blob/main/doc/developer/mzbuild.md
"""

import argparse
import copy
import importlib
import importlib.abc
import importlib.util
import inspect
import os
import re
import subprocess
import sys
import time
from contextlib import contextmanager
from inspect import getmembers, isfunction
from pathlib import Path
from tempfile import TemporaryFile
from typing import (
    Any,
    Callable,
    Dict,
    Iterable,
    Iterator,
    List,
    Literal,
    Optional,
    Sequence,
    Tuple,
    TypedDict,
    TypeVar,
    Union,
    cast,
)

import pg8000
import sqlparse
import yaml
from pg8000 import Cursor

from materialize import mzbuild, spawn, ui
from materialize.ui import UIError

T = TypeVar("T")
say = ui.speaker("C> ")


class UnknownCompositionError(UIError):
    """The specified composition was unknown."""

    def __init__(self, name: str):
        super().__init__(f"unknown composition {name!r}")


class LintError:
    def __init__(self, file: Path, message: str):
        self.file = file
        self.message = message

    def __str__(self) -> str:
        return f"{os.path.relpath(self.file)}: {self.message}"

    def __lt__(self, other: "LintError") -> bool:
        return (self.file, self.message) < (other.file, other.message)


def _lint_composition(path: Path, composition: Any, errors: List[LintError]) -> None:
    if "services" not in composition:
        return

    for (name, service) in composition["services"].items():
        if service.get("mzbuild") == "materialized":
            _lint_materialized_service(path, name, service, errors)
        elif "mzbuild" not in service and "image" in service:
            _lint_image_name(path, service["image"], errors)

        if isinstance(service.get("environment"), dict):
            errors.append(
                LintError(
                    path, f"environment for service {name} uses dict instead of list"
                )
            )


def _lint_image_name(path: Path, spec: str, errors: List[LintError]) -> None:
    from materialize.mzcompose.services import (
        DEFAULT_CONFLUENT_PLATFORM_VERSION,
        LINT_DEBEZIUM_VERSIONS,
    )

    match = re.search(r"((?P<repo>[^/]+)/)?(?P<image>[^:]+)(:(?P<tag>.*))?", spec)
    if not match:
        errors.append(LintError(path, f"malformatted image specification: {spec}"))
        return
    (repo, image, tag) = (match.group("repo"), match.group("image"), match.group("tag"))

    if not tag:
        errors.append(LintError(path, f"image {spec} missing tag"))
    elif tag == "latest":
        errors.append(LintError(path, f'image {spec} depends on floating "latest" tag'))

    if repo == "confluentinc" and image.startswith("cp-"):
        # An '$XXX' environment variable may have been used to specify the version
        if "$" not in tag and tag != DEFAULT_CONFLUENT_PLATFORM_VERSION:
            errors.append(
                LintError(
                    path,
                    f"image {spec} depends on wrong version of Confluent Platform "
                    f"(want {DEFAULT_CONFLUENT_PLATFORM_VERSION})",
                )
            )

    if repo == "debezium":
        if "$" not in tag and tag not in LINT_DEBEZIUM_VERSIONS:
            errors.append(
                LintError(
                    path,
                    f"image {spec} depends on wrong version of Debezium "
                    f"(want {LINT_DEBEZIUM_VERSIONS})",
                )
            )

    if not repo and image == "zookeeper":
        errors.append(
            LintError(
                path, f"replace {spec} with official confluentinc/cp-zookeeper image"
            )
        )

    if repo == "wurstmeister" and image == "kafka":
        errors.append(
            LintError(path, f"replace {spec} with official confluentinc/cp-kafka image")
        )


def _lint_materialized_service(
    path: Path, name: str, service: Any, errors: List[LintError]
) -> None:
    # command may be a string that is passed to the shell, or a list of
    # arguments.
    command = service.get("command", "")
    if isinstance(command, str):
        command = command.split()  # split on whitespace to extract individual arguments
    if "--disable-telemetry" not in command:
        errors.append(
            LintError(
                path,
                "materialized service command does not include --disable-telemetry",
            )
        )
    env = service.get("environment", [])
    if "MZ_DEV=1" not in env:
        errors.append(
            LintError(
                path,
                f"materialized service '{name}' does not specify MZ_DEV=1 in its environment: {env}",
            )
        )


class Composition:
    """A parsed mzcompose.yml with a loaded mzcompose.py file."""

    def __init__(
        self, repo: mzbuild.Repository, name: str, preserve_ports: bool = False
    ):
        self.name = name
        self.repo = repo
        self.preserve_ports = preserve_ports
        self.workflows: Dict[str, Callable[..., None]] = {}

        if name in self.repo.compositions:
            self.path = self.repo.compositions[name]
        else:
            raise UnknownCompositionError(name)

        # load the mzcompose.yml file, if one exists
        mzcompose_yml = self.path / "mzcompose.yml"
        if mzcompose_yml.exists():
            with open(mzcompose_yml) as f:
                compose = yaml.safe_load(f) or {}
        else:
            compose = {}

        self.compose = compose

        if "version" not in compose:
            compose["version"] = "3.7"

        if "services" not in compose:
            compose["services"] = {}

        # Load the mzcompose.py file, if one exists
        mzcompose_py = self.path / "mzcompose.py"
        if mzcompose_py.exists():
            spec = importlib.util.spec_from_file_location("mzcompose", mzcompose_py)
            assert spec
            module = importlib.util.module_from_spec(spec)
            assert isinstance(spec.loader, importlib.abc.Loader)
            spec.loader.exec_module(module)
            for name, fn in getmembers(module, isfunction):
                if name.startswith("workflow_"):
                    # The name of the workflow is the name of the function
                    # with the "workflow_" prefix stripped and any underscores
                    # replaced with dashes.
                    name = name[len("workflow_") :].replace("_", "-")
                    self.workflows[name] = fn

            for python_service in getattr(module, "SERVICES", []):
                compose["services"][python_service.name] = python_service.config

        # Add default volumes
        compose.setdefault("volumes", {}).update(
            {
                "mzdata": None,
                "tmp": None,
                "secrets": None,
            }
        )

        # The CLI driver will handle acquiring these dependencies.
        self.dependencies = self._munge_services(compose["services"].items())

        # Emit the munged configuration to a temporary file so that we can later
        # pass it to Docker Compose.
        self.file = TemporaryFile(mode="w")
        os.set_inheritable(self.file.fileno(), True)
        self._write_compose()

    def _munge_services(
        self, services: List[Tuple[str, dict]]
    ) -> mzbuild.DependencySet:
        images = []

        for name, config in services:
            # Remember any mzbuild references.
            if "mzbuild" in config:
                image_name = config["mzbuild"]
                if image_name not in self.repo.images:
                    raise UIError(f"mzcompose: unknown image {image_name}")
                image = self.repo.images[image_name]
                images.append(image)

            if "propagate_uid_gid" in config:
                if config["propagate_uid_gid"]:
                    config["user"] = f"{os.getuid()}:{os.getgid()}"
                del config["propagate_uid_gid"]

            ports = config.setdefault("ports", [])
            for i, port in enumerate(ports):
                if ":" in str(port):
                    raise UIError(
                        "programming error: disallowed host port in service {name!r}"
                    )
                if self.preserve_ports:
                    # If preserving ports, bind the container port to the same
                    # host port.
                    ports[i] = f"{port}:{port}"

            if self.repo.rd.coverage:
                # Emit coverage information to a file in a directory that is
                # bind-mounted to the "coverage" directory on the host. We
                # inject the configuration to all services for simplicity, but
                # this only have an effect if the service runs instrumented Rust
                # binaries.
                config.setdefault("environment", []).append(
                    f"LLVM_PROFILE_FILE=/coverage/{name}-%m.profraw"
                )
                config.setdefault("volumes", []).append("./coverage:/coverage")

        # Determine mzbuild specs and inject them into services accordingly.
        deps = self.repo.resolve_dependencies(images)
        for _name, config in services:
            if "mzbuild" in config:
                config["image"] = deps[config["mzbuild"]].spec()
                del config["mzbuild"]

        return deps

    def _write_compose(self) -> None:
        self.file.seek(0)
        self.file.truncate()
        yaml.dump(self.compose, self.file)
        self.file.flush()

    @classmethod
    def lint(cls, repo: mzbuild.Repository, name: str) -> List[LintError]:
        """Checks a composition for common errors."""
        if not name in repo.compositions:
            raise UnknownCompositionError(name)

        errs: List[LintError] = []

        path = repo.compositions[name] / "mzcompose.yml"

        if path.exists():
            with open(path) as f:
                composition = yaml.safe_load(f) or {}

            _lint_composition(path, composition, errs)
        return errs

    def invoke(self, *args: str, capture: bool = False) -> subprocess.CompletedProcess:
        """Invoke `docker-compose` on the rendered composition.

        Args:
            args: The arguments to pass to `docker-compose`.
            capture: Whether to capture the child's stdout stream.
        """
        print(f"$ docker-compose {' '.join(args)}", file=sys.stderr)

        self.file.seek(0)

        stdout = None
        if capture:
            stdout = subprocess.PIPE

        try:
            return subprocess.run(
                [
                    "docker-compose",
                    f"-f/dev/fd/{self.file.fileno()}",
                    "--project-directory",
                    self.path,
                    *args,
                ],
                close_fds=False,
                check=True,
                stdout=stdout,
                text=True,
            )
        except subprocess.CalledProcessError as e:
            if e.stdout:
                print(e.stdout)
            raise UIError(f"running docker-compose failed (exit status {e.returncode})")

    def port(self, service: str, private_port: Union[int, str]) -> int:
        """Get the public port for a service's private port.

        Delegates to `docker-compose port`. See that command's help for details.

        Args:
            service: The name of a service in the composition.
            private_port: A private port exposed by the service.
        """
        proc = self.invoke("port", service, str(private_port), capture=True)
        if not proc.stdout.strip():
            raise UIError(
                f"service f{service!r} is not exposing port {private_port!r}",
                hint="is the service running?",
            )
        return int(proc.stdout.split(":")[1])

    def default_port(self, service: str) -> int:
        """Get the default public port for a service.

        Args:
            service: The name of a service in the composition.
        """
        ports = self.compose["services"][service]["ports"]
        if not ports:
            raise UIError(f"service f{service!r} does not expose any ports")
        private_port = str(ports[0]).split(":")[0]
        return self.port(service, private_port)

    def workflow(self, name: str, *args: str) -> None:
        """Run a workflow in the composition.

        Raises a `KeyError` if the workflow does not exist.

        Args:
            name: The name of the workflow to run.
            args: The arguments to pass to the workflow function.
        """
        ui.header(f"Running workflow {name}")
        func = self.workflows[name]
        parser = WorkflowArgumentParser(name, inspect.getdoc(func), list(args))
        if len(inspect.signature(func).parameters) > 1:
            func(self, parser)
        else:
            # If the workflow doesn't have an `args` parameter, parse them here
            # with an empty parser to reject bogus arguments and to handle the
            # trivial help message.
            parser.parse_args()
            func(self)

    @contextmanager
    def override(self, *services: "Service") -> Iterator[None]:
        """Temporarily update the composition with the specified services.

        The services must already exist in the composition. They restored to
        their old definitions when the `with` block ends. Note that the service
        definition is written in its entirety; i.e., the configuration is not
        deep merged but replaced wholesale.

        Lest you are tempted to change this function to allow dynamically
        injecting new services: do not do this! These services will not be
        visible to other commands, like `mzcompose run`, `mzcompose logs`, or
        `mzcompose down`, which makes debugging or inspecting the composition
        challenging.
        """
        # Remember the old composition.
        old_compose = copy.deepcopy(self.compose)

        # Update the composition with the new service definitions.
        deps = self._munge_services([(s.name, cast(dict, s.config)) for s in services])
        for service in services:
            if service.name not in self.compose["services"]:
                raise RuntimeError(
                    "programming error in call to Workflow.with_services: "
                    f"{service.name!r} does not exist"
                )
            self.compose["services"][service.name] = service.config

        # Re-acquire dependencies, as the override may have swapped an `image`
        # config for an `mzbuild` config.
        deps.acquire()

        self._write_compose()

        try:
            # Run the next composition.
            yield
        finally:
            # Restore the old composition.
            self.compose = old_compose
            self._write_compose()

    def sql_cursor(self) -> Cursor:
        """Get a cursor to run SQL queries against the materialized service."""
        port = self.default_port("materialized")
        conn = pg8000.connect(host="localhost", user="materialize", port=port)
        conn.autocommit = True
        return conn.cursor()

    def sql(self, sql: str) -> None:
        """Run a batch of SQL statements against the materialized service."""
        with self.sql_cursor() as cursor:
            for statement in sqlparse.split(sql):
                cursor.execute(statement)

    def start_and_wait_for_tcp(self, services: List[str]) -> None:
        """Sequentially start the named services, waiting for eaach to become
        available via TCP before moving on to the next."""
        for service in services:
            self.up(service)
            for port in self.compose["services"][service].get("ports", []):
                self.wait_for_tcp(host=service, port=port)

    def run(
        self,
        service: str,
        *args: str,
        detach: bool = False,
        rm: bool = False,
        env: Dict[str, str] = {},
        capture: bool = False,
    ) -> subprocess.CompletedProcess:
        """Run a one-off command in a service.

        Delegates to `docker-compose run`. See that command's help for details.
        Note that unlike `docker compose run`, any services whose definitions
        have changed are rebuilt (like `docker-compose up` would do) before the
        command is executed.

        Args:
            service: The name of a service in the composition.
            args: Arguments to pass to the service's entrypoint.
            detach: Run the container in the background.
            env: Additional environment variables to set in the container.
            rm: Remove container after run.
            capture: Capture the stdout of the `docker-compose` invocation.
        """
        # Restart any dependencies whose definitions have changed. The trick,
        # taken from Buildkite's Docker Compose plugin, is to run an `up`
        # command that requests zero instances of the requested service.
        self.invoke("up", "--detach", "--scale", f"{service}=0", service)
        return self.invoke(
            "run",
            *(f"-e{k}={v}" for k, v in env.items()),
            *(["--detach"] if detach else []),
            *(["--rm"] if rm else []),
            service,
            *args,
            capture=capture,
        )

    def up(self, *services: str, detach: bool = True) -> None:
        """Build, (re)create, and start the named services.

        Delegates to `docker-compose up`. See that command's help for details.

        Args:
            services: The names of services in the composition.
            detach: Run containers in the background.
        """
        self.invoke("up", *(["--detach"] if detach else []), *services)

    def kill(self, *services: str, signal: str = "SIGKILL") -> None:
        """Force stop service containers.

        Delegates to `docker-compose kill`. See that command's help for details.

        Args:
            services: The names of services in the composition.
            signal: The signal to deliver.
        """
        self.invoke("kill", f"-s{signal}", *services)

    def rm(
        self, *services: str, stop: bool = True, destroy_volumes: bool = True
    ) -> None:
        """Remove stopped service containers.

        Delegates to `docker-compose rm`. See that command's help for details.

        Args:
            services: The names of services in the composition.
            stop: Stop the containers if necessary.
            destroy_volumes: Destroy any anonymous volumes associated with the
                service. Note that this does not destroy any named volumes
                attached to the service.
        """
        self.invoke(
            "rm",
            "--force",
            *(["--stop"] if stop else []),
            *(["-v"] if destroy_volumes else []),
            *services,
        )

    def rm_volumes(self, *volumes: str, force: bool = False) -> None:
        """Remove the named volumes.

        Args:
            volumes: The names of volumes in the composition.
            force: Whether to force the removal (i.e., don't error if the
                volume does not exist).
        """
        volumes = (f"{self.name}_{v}" for v in volumes)
        spawn.runv(
            ["docker", "volume", "rm", *(["--force"] if force else []), *volumes]
        )

    def sleep(self, duration: float) -> None:
        """Sleep for the specified duration in seconds."""
        print(f"Sleeping for {duration} seconds...")
        time.sleep(duration)

    # TODO(benesch): replace with Docker health checks.
    def wait_for_tcp(
        self,
        *,
        host: str = "localhost",
        port: int,
        timeout_secs: int = 240,
    ) -> None:
        ui.progress(f"waiting for {host}:{port}", "C")
        for remaining in ui.timeout_loop(timeout_secs):
            cmd = f"docker run --rm -t --network {self.name}_default ubuntu:focal-20210723".split()

            try:
                _check_tcp(cmd[:], host, port, timeout_secs)
            except subprocess.CalledProcessError:
                ui.progress(" {}".format(int(remaining)))
            else:
                ui.progress(" success!", finish=True)
                return

        ui.progress(" error!", finish=True)
        raise UIError(f"unable to connect to {host}:{port}")

    # TODO(benesch): replace with Docker health checks.
    def wait_for_postgres(
        self,
        *,
        dbname: str = "postgres",
        port: Optional[int] = None,
        host: str = "localhost",
        timeout_secs: int = 120,
        query: str = "SELECT 1",
        user: str = "postgres",
        password: str = "postgres",
        expected: Union[Iterable[Any], Literal["any"]] = [[1]],
        print_result: bool = False,
        service: str = "postgres",
    ) -> None:
        """Wait for a PostgreSQL service to start.

        Args:
            dbname: the name of the database to wait for
            host: the host postgres is listening on
            port: the port postgres is listening on
            timeout_secs: How long to wait for postgres to be up before failing (Default: 30)
            query: The query to execute to ensure that it is running (Default: "Select 1")
            user: The chosen user (this is only relevant for postgres)
            service: The service that postgres is running as (Default: postgres)
        """
        _wait_for_pg(
            dbname=dbname,
            host=host,
            port=port or self.default_port(service),
            timeout_secs=timeout_secs,
            query=query,
            user=user,
            password=password,
            expected=expected,
            print_result=print_result,
        )

    # TODO(benesch): replace with Docker health checks.
    def wait_for_materialized(
        self,
        service: str = "materialized",
        *,
        user: str = "materialize",
        dbname: str = "materialize",
        host: str = "localhost",
        port: Optional[int] = None,
        timeout_secs: int = 60,
        query: str = "SELECT 1",
        expected: Union[Iterable[Any], Literal["any"]] = [[1]],
        print_result: bool = False,
    ) -> None:
        """Like `Workflow.wait_for_postgres`, but with Materialize defaults."""
        self.wait_for_postgres(
            user=user,
            dbname=dbname,
            host=host,
            port=port,
            timeout_secs=timeout_secs,
            query=query,
            expected=expected,
            print_result=print_result,
            service=service,
        )


class ServiceConfig(TypedDict, total=False):
    """The definition of a service in Docker Compose.

    This object corresponds directly to the YAML definition in a
    docker-compose.yml file, plus two mzcompose-specific attributes. Full
    details are available in [Services top-level element][ref] chapter of the
    Compose Specification.

    [ref]: https://github.com/compose-spec/compose-spec/blob/master/spec.md#services-top-level-element
    """

    mzbuild: str
    """The name of an mzbuild image to dynamically acquire before invoking
    Docker Compose.

    This is a mzcompose-extension to Docker Compose. The image must exist in
    the repository. If `mzbuild` is set, neither `build` nor `image` should be
    set.
    """

    propagate_uid_gid: bool
    """Request that the Docker image be run with the user ID and group ID of the
    host user.

    This is an mzcompose extension to Docker Compose. It is equivalent to
    passing `--user $(id -u):$(id -g)` to `docker run`. The defualt is `False`.
    """

    image: str
    """The name and tag of an image on Docker Hub."""

    hostname: str
    """The hostname to use.

    By default, the name of the service is used as the hostname.
    """

    entrypoint: List[str]
    """Override the entrypoint specified in the image."""

    command: str
    """Override the command specified in the image."""

    init: bool
    """Whether to run an init process in the container."""

    ports: Sequence[Union[int, str]]
    """Service ports to expose to the host."""

    environment: List[str]
    """Additional environment variables to set.

    Each entry must be in the form `NAME=VALUE`.

    TODO(benesch): this should accept a `Dict[str, str]` instead.
    """

    depends_on: List[str]
    """The list of other services that must be started before this one."""

    volumes: List[str]
    """Volumes to attach to the service."""

    networks: Dict[str, Dict[str, List[str]]]
    """Additional networks to join.

    TODO(benesch): this should use a nested TypedDict.
    """

    deploy: Dict[str, Dict[str, Dict[str, str]]]
    """Additional deployment configuration, like resource limits.

    TODO(benesch): this should use a nested TypedDict.
    """


class Service:
    """A Docker Compose service in a `Composition`.

    Attributes:
        name: The name of the service.
        config: The definition of the service.
    """

    def __init__(self, name: str, config: ServiceConfig) -> None:
        self.name = name
        self.config = config


class WorkflowArgumentParser(argparse.ArgumentParser):
    """An argument parser provided to a workflow in a `Composition`.

    You can call `add_argument` and other methods on this argument parser like
    usual. When you are ready to parse arguments, call `parse_args` or
    `parse_known_args` like usual; the argument parser will automatically use
    the arguments that the user provided to the workflow.
    """

    def __init__(self, name: str, description: Optional[str], args: List[str]):
        self.args = args
        super().__init__(prog=f"mzcompose run {name}", description=description)

    def parse_known_args(
        self,
        args: Optional[Sequence[str]] = None,
        namespace: Optional[argparse.Namespace] = None,
    ) -> Tuple[argparse.Namespace, List[str]]:
        if args is None:
            args = self.args
        return super().parse_known_args(args, namespace)


# TODO(benesch): replace with Docker health checks.
def _check_tcp(
    cmd: List[str], host: str, port: int, timeout_secs: int, kind: str = ""
) -> List[str]:
    cmd.extend(
        [
            "timeout",
            str(timeout_secs),
            "bash",
            "-c",
            f"cat < /dev/null > /dev/tcp/{host}/{port}",
        ]
    )
    try:
        spawn.capture(cmd, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as e:
        ui.log_in_automation(
            "wait-for-tcp ({}{}:{}): error running {}: {}, stdout:\n{}\nstderr:\n{}".format(
                kind, host, port, ui.shell_quote(cmd), e, e.stdout, e.stderr
            )
        )
        raise
    return cmd


# TODO(benesch): replace with Docker health checks.
def _wait_for_pg(
    timeout_secs: int,
    query: str,
    dbname: str,
    port: int,
    host: str,
    user: str,
    password: str,
    print_result: bool,
    expected: Union[Iterable[Any], Literal["any"]],
) -> None:
    """Wait for a pg-compatible database (includes materialized)"""
    args = f"dbname={dbname} host={host} port={port} user={user} password={password}"
    ui.progress(f"waiting for {args} to handle {query!r}", "C")
    error = None
    for remaining in ui.timeout_loop(timeout_secs):
        try:
            conn = pg8000.connect(
                database=dbname,
                host=host,
                port=port,
                user=user,
                password=password,
                timeout=1,
            )
            # The default (autocommit = false) wraps everything in a transaction.
            conn.autocommit = True
            cur = conn.cursor()
            cur.execute(query)
            if expected == "any" and cur.rowcount == -1:
                ui.progress("success!", finish=True)
                return
            result = list(cur.fetchall())
            if expected == "any" or result == expected:
                if print_result:
                    say(f"query result: {result}")
                else:
                    ui.progress("success!", finish=True)
                return
            else:
                say(
                    f"host={host} port={port} did not return rows matching {expected} got: {result}"
                )
        except Exception as e:
            ui.progress(" " + str(int(remaining)))
            error = e
    ui.progress(finish=True)
    raise UIError(f"never got correct result for {args}: {error}")
