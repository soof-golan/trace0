# tracezero

`tracezero` is [trace0](https://pypi.org/project/trace0/) under its longer
name. Installing it installs `trace0` at the matching version and offers the
same API and CLI:

```bash
uvx tracezero run --output trace.pb your_script.py
```

```python
from tracezero import Tracer

with Tracer("trace.pb"):
    your_workload()
```

Everything else — documentation, issues, source — lives at
<https://github.com/soof-golan/trace0>.
