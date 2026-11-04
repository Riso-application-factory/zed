"""Command dispatcher for the riso upstream synchronization pipeline."""

import argparse
import importlib
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for name in ["riso_adr", "riso_triage", "riso_prepare", "riso_claude"]:
        try:
            module = importlib.import_module(name)
        except ModuleNotFoundError as error:
            if error.name != name:
                raise
            continue
        module.register(subparsers)
    arguments = parser.parse_args()
    sys.exit(arguments.func(arguments) or 0)


if __name__ == "__main__":
    main()
