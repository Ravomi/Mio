"""On-disk state for the fixture build pipeline."""
import json
import os


def load_state(path):
    """Return the persisted build state."""
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def save_state(path, state):
    """Persist the build state atomically."""
    directory = os.path.dirname(path)
    if directory:
        os.makedirs(directory, exist_ok=True)
    temporary = path + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(state, handle, sort_keys=True)
    os.replace(temporary, path)
