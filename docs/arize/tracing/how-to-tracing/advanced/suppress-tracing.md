> ## Documentation Index
> Fetch the complete documentation index at: https://arizeai-433a7140.mintlify.app/llms.txt
> Use this file to discover all available pages before exploring further.

# Suppress Tracing

## How to turn off tracing

Tracing can be paused temporarily or disabled permanently.

### Pause tracing using context manager

If there is a section of your code for which tracing is not desired, e.g. the document chunking process, it can be put inside the `suppress_tracing` context manager as shown below.

```python theme={null}
from phoenix.trace import suppress_tracing

with suppress_tracing():
    # Code running inside this block doesn't generate traces.
    # For example, running LLM evals here won't generate additional traces.
    ...
# Tracing will resume outside the block.
...
```

### Uninstrument the auto-instrumentors permanently

Calling `.uninstrument()` on the auto-instrumentors will remove tracing permanently. Below is the examples for LangChain, LlamaIndex and OpenAI, respectively.

```python theme={null}
LangChainInstrumentor().uninstrument()
LlamaIndexInstrumentor().uninstrument()
OpenAIInstrumentor().uninstrument()
# etc.
```
