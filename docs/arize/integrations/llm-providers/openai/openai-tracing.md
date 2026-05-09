> ## Documentation Index
> Fetch the complete documentation index at: https://arizeai-433a7140.mintlify.app/llms.txt
> Use this file to discover all available pages before exploring further.

# OpenAI Tracing

> Phoenix provides auto-instrumentation for the [OpenAI Python Library](https://github.com/openai/openai-python).

<Info>
  **Note**\*: This instrumentation also works with Azure OpenAI
</Info>

## Install

```bash theme={null}
pip install openinference-instrumentation-openai openai
```

## Setup

Add your OpenAI API key as an environment variable:

```shell theme={null}
export OPENAI_API_KEY=[your_key_here]
```

Use the register function to connect your application to Phoenix:

```python theme={null}
from phoenix.otel import register

# configure the Phoenix tracer
tracer_provider = register(
  project_name="my-llm-app", # Default is 'default'
  auto_instrument=True # Auto-instrument your app based on installed dependencies
)
```

## Run OpenAI

```python theme={null}
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Write a haiku."}],
)
print(response.choices[0].message.content)
```

## Observe

Now that you have tracing setup, all invocations of OpenAI (completions, chat completions, embeddings) will be streamed to your running Phoenix for observability and evaluation.

## Resources

* [Example notebook](https://github.com/Arize-ai/phoenix/blob/main/tutorials/tracing/openai_tracing_tutorial.ipynb)

* [OpenInference package](https://github.com/Arize-ai/openinference/tree/main/python/instrumentation/openinference-instrumentation-openai)

* [Working examples](https://github.com/Arize-ai/openinference/tree/main/python/instrumentation/openinference-instrumentation-openai/examples)
