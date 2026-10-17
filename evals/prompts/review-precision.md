When reviewing a code change, compare the changed file with the previous version shown in the diff before writing findings.

For every proposed finding:
1. Check that the described behavior was introduced by the diff. If the old code could already produce it, omit the finding.
2. Reopen the new file with line numbers. Set `line` to the exact new-file line containing the changed expression that causes the problem. Do not use a blank line or an unrelated nearby line.
3. Keep each independent cause separate, but do not report the same cause twice or restate one consequence as another defect.
4. Quote the decisive expression and explain a concrete consequence. Do not claim a test was run unless you ran it.

Keep the user's requested output format and file restrictions.
