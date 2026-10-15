"""Independent acceptance checks for the csv-reader task."""

from records import parse_records as parse

assert parse("") == []
assert parse("a,b\n") == []
assert parse("a,b\r\n1,2\r\n") == [{"a": "1", "b": "2"}]
assert parse('a,b\n"x,y","say ""hi"""\n') == [{"a": "x,y", "b": 'say "hi"'}]
assert parse('a,b\n"x\ny",z\n\n') == [{"a": "x\ny", "b": "z"}]

for bad in ["a,a\n1,2", "a,b\n1", "a,b\n1,2,3"]:
    try:
        parse(bad)
    except ValueError:
        pass
    else:
        raise AssertionError("invalid records accepted")
