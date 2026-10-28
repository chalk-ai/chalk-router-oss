import OpenAI from 'openai';
import * as dotenv from 'dotenv';

dotenv.config();

const baseURL = process.env.ROUTER_BASE_URL || 'http://127.0.0.1:4001/v1';

const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL,
});

// Models to test - configure via environment or use defaults
// For GPT-4o and newer models, use max_completion_tokens instead of max_tokens
const OPENAI_MODELS = process.env.OPENAI_TEST_MODELS?.split(',') || ['gpt-5.2', 'gpt-5.1', 'gpt-5'];
const ANTHROPIC_MODELS = process.env.ANTHROPIC_TEST_MODELS?.split(',') || [
  'anthropic/claude-sonnet-4-5-20250929',
  'anthropic/claude-haiku-4-5-20251001',
  'anthropic/claude-opus-4-5-20251101'
];
const ALL_MODELS = [...OPENAI_MODELS, ...ANTHROPIC_MODELS];

// Models that require max_completion_tokens instead of max_tokens
const USES_MAX_COMPLETION_TOKENS = ['gpt-4o', 'gpt-4o-mini', 'gpt-5', 'gpt-5.2', 'gpt-5.1', 'gpt-5.0'];

function getMaxTokensParam(model: string, tokens: number) {
  if (USES_MAX_COMPLETION_TOKENS.some(m => model.includes(m))) {
    return { max_completion_tokens: tokens };
  }
  return { max_tokens: tokens };
}

// Test results tracking
let passedTests = 0;
let failedTests = 0;
const failures: Array<{ test: string; model: string; error: string }> = [];

function logSuccess(test: string, model: string) {
  console.log(`  ✓ ${test} (${model})`);
  passedTests++;
}

function logFailure(test: string, model: string, error: any) {
  console.log(`  ✗ ${test} (${model})`);
  console.log(`    Error: ${error.message || error}`);
  failedTests++;
  failures.push({ test, model, error: error.message || String(error) });
}

async function testBasicChat() {
  console.log('\n=== Testing Basic Chat Completion ===');
  try {
    const response = await client.chat.completions.create({
      model: 'gpt-4',
      messages: [
        { role: 'user', content: 'Say hello!' }
      ],
      max_tokens: 50,
    });

    console.log('Chat response:', response.choices[0].message.content);
    logSuccess('Basic chat completion', 'gpt-4');
  } catch (error) {
    logFailure('Basic chat completion', 'gpt-4', error);
  }
}

async function testEmbeddings() {
  console.log('\n=== Testing Embeddings ===');
  try {
    const response = await client.embeddings.create({
      model: 'text-embedding-ada-002',
      input: 'The quick brown fox jumps over the lazy dog',
    });

    console.log('Embedding dimensions:', response.data[0].embedding.length);
    logSuccess('Embeddings', 'text-embedding-ada-002');
  } catch (error) {
    logFailure('Embeddings', 'text-embedding-ada-002', error);
  }
}

async function testToolCalling(model: string) {
  try {
    const response = await client.chat.completions.create({
      model,
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
                unit: {
                  type: 'string',
                  enum: ['celsius', 'fahrenheit'],
                  description: 'The unit of temperature to return',
                },
              },
              required: ['location'],
            },
          },
        },
      ],
      ...getMaxTokensParam(model, 200),
    });

    const message = response.choices[0].message;

    // Check if the model made a tool call
    if (message.tool_calls && message.tool_calls.length > 0) {
      const toolCall = message.tool_calls[0];

      // Verify the tool call structure
      if (toolCall.type === 'function' &&
          toolCall.function.name === 'get_weather' &&
          toolCall.function.arguments) {

        // Parse and validate the arguments
        const args = JSON.parse(toolCall.function.arguments);
        if (args.location) {
          logSuccess('Tool calling', model);
          return;
        }
      }
    }

    throw new Error('No valid tool call was made');
  } catch (error) {
    logFailure('Tool calling', model, error);
  }
}

async function testStructuredOutput(model: string) {
  try {
    const response = await client.chat.completions.create({
      model,
      messages: [
        { role: 'user', content: 'Extract the following information: Name is John Doe, age 30, email john@example.com' }
      ],
      response_format: {
        type: 'json_schema',
        json_schema: {
          name: 'user_info',
          strict: true,
          schema: {
            type: 'object',
            properties: {
              name: {
                type: 'string',
                description: 'The full name of the person',
              },
              age: {
                type: 'number',
                description: 'The age of the person',
              },
              email: {
                type: 'string',
                description: 'The email address',
              },
            },
            required: ['name', 'age', 'email'],
            additionalProperties: false,
          },
        },
      },
      ...getMaxTokensParam(model, 200),
    });

    const content = response.choices[0].message.content;

    // Verify we got JSON output
    if (content) {
      const parsed = JSON.parse(content);

      // Verify the structure matches our schema
      if (parsed.name && parsed.age && parsed.email) {
        logSuccess('Structured output', model);
        return;
      }
    }

    throw new Error('Invalid structured output format');
  } catch (error) {
    logFailure('Structured output', model, error);
  }
}

async function testToolCallingAllModels() {
  console.log('\n=== Testing Tool Calling Across All Models ===');

  for (const model of ALL_MODELS) {
    await testToolCalling(model);
  }
}

async function testStructuredOutputAllModels() {
  console.log('\n=== Testing Structured Output Across All Models ===');

  for (const model of ALL_MODELS) {
    await testStructuredOutput(model);
  }
}

async function testComplexToolCalling(model: string) {
  try {
    const response = await client.chat.completions.create({
      model,
      messages: [
        { role: 'user', content: 'What is 25 + 17, and what is the square root of 144?' }
      ],
      tools: [
        {
          type: 'function',
          function: {
            name: 'calculate',
            description: 'Perform a mathematical calculation',
            parameters: {
              type: 'object',
              properties: {
                operation: {
                  type: 'string',
                  enum: ['add', 'subtract', 'multiply', 'divide', 'sqrt', 'power'],
                  description: 'The mathematical operation to perform',
                },
                operands: {
                  type: 'array',
                  items: { type: 'number' },
                  description: 'The numbers to operate on',
                },
              },
              required: ['operation', 'operands'],
            },
          },
        },
      ],
      ...getMaxTokensParam(model, 300),
    });

    const message = response.choices[0].message;

    // Check if the model made tool calls (should be 2 for both calculations)
    if (message.tool_calls && message.tool_calls.length > 0) {
      let validCalls = 0;

      for (const toolCall of message.tool_calls) {
        if (toolCall.type === 'function' &&
            toolCall.function.name === 'calculate' &&
            toolCall.function.arguments) {
          const args = JSON.parse(toolCall.function.arguments);
          if (args.operation && args.operands && Array.isArray(args.operands)) {
            validCalls++;
          }
        }
      }

      if (validCalls > 0) {
        logSuccess('Complex tool calling', model);
        return;
      }
    }

    throw new Error('No valid tool calls were made');
  } catch (error) {
    logFailure('Complex tool calling', model, error);
  }
}

async function testComplexToolCallingAllModels() {
  console.log('\n=== Testing Complex Tool Calling (Multiple Calls) ===');

  for (const model of ALL_MODELS) {
    await testComplexToolCalling(model);
  }
}

async function testNestedStructuredOutput(model: string) {
  try {
    const response = await client.chat.completions.create({
      model,
      messages: [
        {
          role: 'user',
          content: 'Create a company profile with: Company name: Acme Corp, Founded: 2020, CEO: Jane Smith (age 45, email jane@acme.com), CTO: Bob Johnson (age 38, email bob@acme.com)'
        }
      ],
      response_format: {
        type: 'json_schema',
        json_schema: {
          name: 'company_profile',
          strict: true,
          schema: {
            type: 'object',
            properties: {
              company_name: { type: 'string' },
              founded_year: { type: 'number' },
              executives: {
                type: 'array',
                items: {
                  type: 'object',
                  properties: {
                    name: { type: 'string' },
                    title: { type: 'string' },
                    age: { type: 'number' },
                    email: { type: 'string' },
                  },
                  required: ['name', 'title', 'age', 'email'],
                  additionalProperties: false,
                },
              },
            },
            required: ['company_name', 'founded_year', 'executives'],
            additionalProperties: false,
          },
        },
      },
      ...getMaxTokensParam(model, 300),
    });

    const content = response.choices[0].message.content;

    if (content) {
      const parsed = JSON.parse(content);

      // Verify nested structure
      if (parsed.company_name &&
          parsed.founded_year &&
          parsed.executives &&
          Array.isArray(parsed.executives) &&
          parsed.executives.length >= 2 &&
          parsed.executives[0].name &&
          parsed.executives[0].email) {
        logSuccess('Nested structured output', model);
        return;
      }
    }

    throw new Error('Invalid nested structured output format');
  } catch (error) {
    logFailure('Nested structured output', model, error);
  }
}

async function testNestedStructuredOutputAllModels() {
  console.log('\n=== Testing Nested Structured Output ===');

  for (const model of ALL_MODELS) {
    await testNestedStructuredOutput(model);
  }
}

function printSummary() {
  console.log('\n' + '='.repeat(60));
  console.log('TEST SUMMARY');
  console.log('='.repeat(60));
  console.log(`Total Passed: ${passedTests}`);
  console.log(`Total Failed: ${failedTests}`);
  console.log(`Success Rate: ${((passedTests / (passedTests + failedTests)) * 100).toFixed(1)}%`);

  if (failures.length > 0) {
    console.log('\n' + '='.repeat(60));
    console.log('FAILURES');
    console.log('='.repeat(60));
    for (const failure of failures) {
      console.log(`\n${failure.test} - ${failure.model}`);
      console.log(`  ${failure.error}`);
    }
  }

  console.log('\n' + '='.repeat(60));
}

async function main() {
  console.log('Starting OpenAI Router Integration Tests');
  console.log(`Testing against: ${baseURL}`);
  console.log(`Models: ${ALL_MODELS.join(', ')}`);

  // Basic functionality tests
  await testBasicChat();
  await testEmbeddings();

  // Tool calling tests
  await testToolCallingAllModels();
  await testComplexToolCallingAllModels();

  // Structured output tests
  await testStructuredOutputAllModels();
  await testNestedStructuredOutputAllModels();

  // Print summary
  printSummary();

  // Exit with error code if any tests failed
  if (failedTests > 0) {
    process.exit(1);
  }
}

main();
