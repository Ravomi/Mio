Review the change in `change.diff` against the current `invoice.py`. Write your review to `review.json` only; do not edit the source or diff. Use this JSON shape:

```json
{"findings":[{"severity":"high|medium|low","confidence":0.0,"path":"invoice.py","line":1,"evidence":"specific code and consequence","explanation":"why this is a bug"}],"conclusion":"short summary"}
```

Report only actionable defects introduced by the change. Use a line in the new file for each finding, one finding per independent defect. If there are no defects, use an empty `findings` array. Do not report style preferences.
