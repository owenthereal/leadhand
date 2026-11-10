import argparse
import importlib
import sys

from leadhand import __version__

COMMAND_MODULES: list[str] = ["leadhand.deck", "leadhand.route", "leadhand.dispatch", "leadhand.engine"]


def add_commands(sub) -> None:
    p = sub.add_parser("version", help="print the leadhand version")
    p.set_defaults(func=lambda args: print(f"leadhand {__version__}") or 0)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="leadhand")
    sub = parser.add_subparsers(dest="command", required=True)
    add_commands(sub)
    for name in COMMAND_MODULES:
        importlib.import_module(name).add_commands(sub)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(sys.argv[1:] if argv is None else argv)
    return args.func(args)
