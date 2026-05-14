import sys

from useful_tracer._core import cli_main


def main() -> int:
    return cli_main(sys.argv)


if __name__ == "__main__":
    raise SystemExit(main())
