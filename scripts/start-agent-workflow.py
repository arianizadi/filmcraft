#!/usr/bin/env python3
"""Launch FilmCraft and expose an authenticated MCP bridge without printing its token."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import stat
import subprocess
import sys
import time


class WorkflowError(Exception):
    """A recoverable setup or connection problem."""


def support_directory(path: Path) -> Path:
    path = path.expanduser().absolute()
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise WorkflowError("Agent support directory must be a real directory owned by you")
    os.chmod(path, 0o700)
    return path


def private_open(path: Path, flags: int) -> int:
    descriptor = os.open(path, flags | os.O_NOFOLLOW, 0o600)
    info = os.fstat(descriptor)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        os.close(descriptor)
        raise WorkflowError(f"{path.name} must be a regular file owned by you")
    if stat.S_IMODE(info.st_mode) != 0o600:
        os.close(descriptor)
        raise WorkflowError(f"{path.name} must have owner-only permissions (chmod 600)")
    return descriptor


def token_for(directory: Path, create: bool = False) -> str:
    path = directory / "control-token"
    if create:
        try:
            descriptor = private_open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL)
        except FileExistsError:
            pass
        else:
            with os.fdopen(descriptor, "w", encoding="ascii") as stream:
                stream.write(secrets.token_hex(32) + "\n")
                stream.flush()
                os.fsync(stream.fileno())
    try:
        descriptor = private_open(path, os.O_RDONLY)
    except FileNotFoundError as error:
        raise WorkflowError("No pairing token exists; run this helper with start first") from error
    with os.fdopen(descriptor, "r", encoding="ascii") as stream:
        raw = stream.read(67)
    token = raw.removesuffix("\n")
    if len(raw) not in (64, 65) or len(token) != 64 or any(character not in "0123456789abcdefABCDEF" for character in token):
        raise WorkflowError("Stored pairing token is invalid; remove control-token and start again")
    return token


def executable(path: Path, name: str) -> Path:
    path = path.expanduser().resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise WorkflowError(f"{name} executable is unavailable: {path}")
    return path


def binaries(args: argparse.Namespace) -> tuple[Path, Path]:
    root = Path(__file__).resolve().parent.parent
    app_candidates = [
        root / "target/macos-package/FilmCraft.app/Contents/MacOS/FilmCraft",
        root / "target/release/filmcraft",
        root / "target/aarch64-apple-darwin/release/filmcraft",
        root / "target/x86_64-apple-darwin/release/filmcraft",
        Path("/Applications/FilmCraft.app/Contents/MacOS/FilmCraft"),
    ]
    if args.app:
        app = Path(args.app)
        if app.suffix == ".app":
            app = app / "Contents/MacOS/FilmCraft"
    else:
        app = next((path for path in app_candidates if path.is_file()), app_candidates[0])
    app = executable(app, "FilmCraft")
    cli_candidates = [
        app.parent / "filmcraft-cli",
        root / "target/macos-package/bin/filmcraft-cli",
        root / "target/release/filmcraft-cli",
        root / "target/aarch64-apple-darwin/release/filmcraft-cli",
        root / "target/x86_64-apple-darwin/release/filmcraft-cli",
    ]
    cli = Path(args.cli) if args.cli else next((path for path in cli_candidates if path.is_file()), cli_candidates[0])
    return app, executable(cli, "filmcraft-cli")


def server_session(port: int, token: str, timeout: float = 2.0) -> dict:
    request = {
        "id": 1,
        "token": token,
        "method": "engine.execute",
        "params": {"command": "agent.session", "params": {}},
    }
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as connection:
        connection.settimeout(timeout)
        connection.sendall((json.dumps(request) + "\n").encode("utf-8"))
        with connection.makefile("rb") as stream:
            line = stream.readline(65_537)
    if not line.endswith(b"\n") or len(line) > 65_536:
        raise WorkflowError("Control server returned an invalid or oversized reply")
    try:
        response = json.loads(line)
    except (ValueError, UnicodeError, RecursionError) as error:
        raise WorkflowError("The process on this port is not an authenticated FilmCraft server") from error
    result = response.get("result") if isinstance(response, dict) else None
    if not isinstance(result, dict) or not response.get("ok") or response.get("id") != 1 or not isinstance(result.get("sessionId"), str):
        raise WorkflowError("The process on this port did not authenticate as a current FilmCraft app")
    return result


def write_connection(directory: Path, data: dict) -> None:
    # An owner-only directory and exclusive temporary file prevent partial or symlink writes.
    temporary = directory / f"connection-{os.getpid()}-{secrets.token_hex(8)}.tmp"
    try:
        descriptor = private_open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL)
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(data, stream)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, directory / "connection.json")
    finally:
        temporary.unlink(missing_ok=True)


def paired_session(directory: Path, port: int, token: str) -> dict:
    try:
        descriptor = private_open(directory / "connection.json", os.O_RDONLY)
    except FileNotFoundError as error:
        raise WorkflowError("No helper-launched app is paired; run start first") from error
    try:
        with os.fdopen(descriptor, "r", encoding="utf-8") as stream:
            record = json.loads(stream.read(65_537))
    except (ValueError, UnicodeError, RecursionError) as error:
        raise WorkflowError("Pairing record is invalid; close FilmCraft and run start again") from error
    if not isinstance(record, dict) or record.get("port") != port:
        raise WorkflowError("Port does not match the helper-launched app; use its original --port")
    current = server_session(port, token)
    if current["sessionId"] != record.get("sessionId"):
        raise WorkflowError("A different app session owns this port; close it and run start again")
    return current


def start(args: argparse.Namespace, directory: Path, app: Path, cli: Path) -> None:
    # Refuse an occupied port even when a previous FilmCraft instance might own it. Never attach
    # silently to whichever project happens to be open in another process.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reservation:
        try:
            reservation.bind(("127.0.0.1", args.port))
        except OSError as error:
            raise WorkflowError(f"Port {args.port} is occupied; close its app or choose --port before starting") from error
    token = token_for(directory, create=True)
    environment = dict(os.environ, FILMCRAFT_CONTROL_TOKEN=token)
    command = [str(app), "--control", str(args.port)]
    if args.project:
        project = Path(args.project).expanduser().resolve()
        if not project.is_file() or project.suffix.lower() != ".fcproj":
            raise WorkflowError("--project must name an existing .fcproj file")
        command.append(str(project))
    descriptor = private_open(directory / "desktop.log", os.O_WRONLY | os.O_CREAT | os.O_APPEND)
    with os.fdopen(descriptor, "ab") as log:
        process = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    deadline = time.monotonic() + args.startup_timeout
    last_error = "The control server has not answered yet"
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise WorkflowError(f"FilmCraft exited with status {process.returncode}; see {directory / 'desktop.log'}")
        try:
            session = server_session(args.port, token, timeout=0.75)
        except (OSError, WorkflowError) as error:
            last_error = str(error)
            time.sleep(0.2)
            continue
        write_connection(directory, {"port": args.port, "sessionId": session["sessionId"], "pid": process.pid, "app": str(app), "cli": str(cli)})
        print(f"FilmCraft is ready on 127.0.0.1:{args.port} ({session.get('permission', 'unknown')} permission).")
        print("Review proposals in Window > Agent Review. Run this helper with config for a secret-free MCP stanza.")
        return
    raise WorkflowError(f"FilmCraft (PID {process.pid}) remains open but its control server is not ready: {last_error}; see {directory / 'desktop.log'}. Save and close that instance before retrying")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("start", "mcp", "config", "status"), nargs="?", default="start")
    parser.add_argument("--app", help="absolute FilmCraft.app or desktop executable path")
    parser.add_argument("--cli", help="absolute filmcraft-cli executable path")
    parser.add_argument("--port", type=int, default=9876)
    parser.add_argument("--project", help="project file to open when starting the app")
    parser.add_argument("--startup-timeout", type=float, default=30.0)
    parser.add_argument("--support-dir", type=Path, default=Path.home() / "Library/Application Support/FilmCraft/Agent")
    args = parser.parse_args()
    if not 1 <= args.port <= 65535 or not 1.0 <= args.startup_timeout <= 60.0:
        parser.error("--port must be 1..65535 and --startup-timeout must be 1..60 seconds")
    try:
        if args.mode == "config":
            app, cli = binaries(args)
            arguments = [str(Path(__file__).resolve()), "mcp", "--app", str(app), "--cli", str(cli), "--port", str(args.port), "--support-dir", str(args.support_dir.expanduser().absolute())]
            print(json.dumps({"mcpServers": {"filmcraft": {"command": str(Path(sys.executable).resolve()), "args": arguments}}}, indent=2))
            return 0
        directory = support_directory(args.support_dir)
        if args.mode == "start":
            app, cli = binaries(args)
            start(args, directory, app, cli)
            return 0
        token = token_for(directory)
        session = paired_session(directory, args.port, token)
        if args.mode == "status":
            print(json.dumps(session, indent=2))
            return 0
        _, cli = binaries(args)
        environment = dict(os.environ, FILMCRAFT_CONTROL_TOKEN=token)
        os.execve(str(cli), [str(cli), "mcp", "--bridge", f"127.0.0.1:{args.port}"], environment)
    except (WorkflowError, OSError, UnicodeError) as error:
        print(f"FilmCraft setup: {error}", file=sys.stderr)
        return 1
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
