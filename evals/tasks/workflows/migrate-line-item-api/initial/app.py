"""Tiny CLI: print the monthly total for a JSON list of line items."""
import json
import sys

from invoicing.report import monthly_total


def main(argv):
    items = json.loads(argv[1])
    print(monthly_total(items))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
