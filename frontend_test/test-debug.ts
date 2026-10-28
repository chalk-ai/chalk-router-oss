import OpenAI from 'openai';
import * as dotenv from 'dotenv';

dotenv.config();

const baseURL = process.env.ROUTER_BASE_URL || 'http://localhost:4001/v1';

const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL,
});

async function testToolCall() {
  console.log('=== Testing Tool Call with gpt-5.2 ===\n');

  const response = await client.chat.completions.create({
    model: 'gpt-5.2',
    messages: [
      { role: 'user', content: 'What is the weather in San Francisco?' }
    ],
    tools: [
      {
        type: 'function',
        function: {
          name: 'get_weather',
          description: 'Get the current weather in a given location',
          parameters: {
            type: 'object',
            properties: {
              location: {
                type: 'string',
                description: 'The city and state, e.g. San Francisco, CA',
              },
            },
            required: ['location'],
          },
        },
      },
    ],
    max_completion_tokens: 200,
  });

  console.log('Full response:', JSON.stringify(response, null, 2));
  console.log('\nMessage:', response.choices[0].message);
  console.log('\nTool calls:', response.choices[0].message.tool_calls);
}

async function testStructuredOutput() {
  console.log('\n\n=== Testing Structured Output with gpt-5.2 ===\n');

  const response = await client.chat.completions.create({
    model: 'gpt-5.2',
    messages: [
      { role: 'user', content: 'Extract: Name is John Doe, age 30' }
    ],
    response_format: {
      type: 'json_schema',
      json_schema: {
        name: 'user_info',
        strict: true,
        schema: {
          type: 'object',
          properties: {
            name: { type: 'string' },
            age: { type: 'number' },
          },
          required: ['name', 'age'],
          additionalProperties: false,
        },
      },
    },
    max_completion_tokens: 200,
  });

  console.log('Full response:', JSON.stringify(response, null, 2));
  console.log('\nContent:', response.choices[0].message.content);
}

async function main() {
  await testToolCall();
  await testStructuredOutput();
}

main();
