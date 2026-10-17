"""Display labels for a sequence of objects with a string `name` attribute."""


def labels(items):
    return [item.name.strip().lower() for item in items]
