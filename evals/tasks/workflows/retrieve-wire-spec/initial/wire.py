"""Wire record parser used by the ingest tools."""


def parse(text):
    records = []
    for line in text.splitlines():
        if not line:
            continue
        record = {}
        for pair in line.split("|"):
            key, _, value = pair.partition("=")
            record[key.upper()] = value
        records.append(record)
    return records


def render(records):
    lines = []
    for record in records:
        lines.append("|".join("{}={}".format(key, value) for key, value in record.items()))
    return "\n".join(lines) + "\n"
