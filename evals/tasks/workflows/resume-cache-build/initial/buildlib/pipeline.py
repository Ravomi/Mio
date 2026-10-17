"""Deterministic fixture pipeline: normalize input files into artifacts."""
import os

from . import cache


def normalize(text):
    """Lower-case and collapse every run of whitespace to a single space."""
    return " ".join(text.split()).lower()


def build_all(inputs_dir, state_path, out_dir):
    """Normalize every input file and record the finished work in state.

    Returns the sorted list of artifact file names written by this run.
    """
    state = cache.load_state(state_path)
    done = set(state.get("done", []))
    os.makedirs(out_dir, exist_ok=True)
    written = []
    for name in sorted(os.listdir(inputs_dir)):
        if not name.endswith(".txt"):
            continue
        with open(os.path.join(inputs_dir, name), encoding="utf-8") as handle:
            body = normalize(handle.read())
        artifact = name[:-4] + ".out"
        with open(os.path.join(out_dir, artifact), "w", encoding="utf-8") as handle:
            handle.write(body + "\n")
        written.append(artifact)
        done.add(name)
    cache.save_state(state_path, {"done": sorted(done)})
    return written
