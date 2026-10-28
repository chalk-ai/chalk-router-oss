import OpenAI from 'openai';
import * as dotenv from 'dotenv';

dotenv.config();

const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  // Using OpenAI directly, not the router
});

async function testToolCall() {
  console.log('=== Testing Tool Call DIRECTLY with OpenAI ===\n');

  try {
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
    console.log('\nTool calls:', response.choices[0].message.tool_calls);
  } catch (error: any) {
    console.error('Error:', error.message);
  }
}

async function testStructuredOutput() {
  console.log('\n\n=== Testing Structured Output DIRECTLY with OpenAI ===\n');

  try {
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

    // Try to parse it
    if (response.choices[0].message.content) {
      const parsed = JSON.parse(response.choices[0].message.content);
      console.log('\nParsed:', parsed);
    }
  } catch (error: any) {
    console.error('Error:', error.message);
  }
}

async function main() {
  await testToolCall();
  await testStructuredOutput();
}

main();
