def parse_records(text):
    lines = text.splitlines()
    keys = lines[0].split(',')
    return [dict(zip(keys, line.split(','))) for line in lines[1:]]
