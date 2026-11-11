import argparse
import importlib
import sys
from pathlib import Path

from leadhand import __version__

COMMAND_MODULES: list[str] = ["leadhand.deck", "leadhand.route", "leadhand.dispatch", "leadhand.engine", "leadhand.launch",
                                "leadhand.status", "leadhand.report"]


def add_commands(sub) -> None:
    p = sub.add_parser("version", help="print the leadhand version")
    p.set_defaults(func=lambda args: print(f"leadhand {__version__}") or 0)

    p = sub.add_parser("unpark", help="let a parked card continue, with a note for its worker")
    p.add_argument("run_dir")
    p.add_argument("--card", type=int, required=True)
    p.add_argument("--note", required=True, help="the answer or instruction for the card; - reads stdin")
    p.set_defaults(func=_cmd_unpark)


def _cmd_unpark(args) -> int:
    from leadhand import engine
    note = sys.stdin.read() if args.note == "-" else args.note
    try:
        engine.unpark(Path(args.run_dir), card=args.card, note=note)
    except (engine.EngineError, OSError, ValueError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(f"card {args.card} unparked")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="leadhand")
    sub = parser.add_subparsers(dest="command", required=True)
    add_commands(sub)
    for name in COMMAND_MODULES:
        importlib.import_module(name).add_commands(sub)
    return parser


def main(argv: list[str] | None = None) -> int:
    from leadhand.launch import close_inherited_lock
    close_inherited_lock()
    args = build_parser().parse_args(sys.argv[1:] if argv is None else argv)
    return args.func(args)
