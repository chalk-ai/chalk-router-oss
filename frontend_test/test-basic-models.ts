import OpenAI from 'openai';
import * as dotenv from 'dotenv';

dotenv.config();

const baseURL = process.env.ROUTER_BASE_URL || 'http://localhost:4001/v1';

const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL,
});

// Models to test
const MODELS_TO_TEST = [
  // GPT-5 variants
  { name: 'gpt-5', displayName: 'GPT-5' },
  { name: 'gpt-5.2', displayName: 'GPT-5.2' },
  { name: 'gpt-5.1', displayName: 'GPT-5.1' },
  { name: 'gpt-5.0', displayName: 'GPT-5.0' },

  // Anthropic models with various name formats
  { name: 'anthropic/claude-3-5-haiku-20241022', displayName: 'Claude 3.5 Haiku' },
  { name: 'anthropic/claude-3-haiku-20240307', displayName: 'Claude 3 Haiku (alt)' },
  { name: 'anthropic/claude-haiku-4.5', displayName: 'Claude Haiku 4.5' },

  { name: 'anthropic/claude-3-5-sonnet-20241022', displayName: 'Claude 3.5 Sonnet' },
  { name: 'anthropic/claude-3-5-sonnet-20240620', displayName: 'Claude 3.5 Sonnet (alt)' },
  { name: 'anthropic/claude-sonnet-4.5', displayName: 'Claude Sonnet 4.5' },

  { name: 'anthropic/claude-3-opus-20240229', displayName: 'Claude 3 Opus' },
  { name: 'anthropic/claude-opus-4.5', displayName: 'Claude Opus 4.5' },
];

async function testModel(modelName: string, displayName: string) {
  try {
    console.log(`\nTesting ${displayName} (${modelName})...`);

    const response = await client.chat.completions.create({
      model: modelName,
      messages: [
        { role: 'user', content: 'Say "Hello, I am working!" and nothing else.' }
      ],
      max_completion_tokens: 50,
    });

    const content = response.choices[0].message.content;
    console.log(`  ✓ Success!`);
    console.log(`  Response: ${content}`);
    console.log(`  Model returned: ${response.model}`);

    return true;
  } catch (error: any) {
    console.log(`  ✗ Failed!`);
    if (error.status) {
      console.log(`  Status: ${error.status}`);
    }
    console.log(`  Error: ${error.message || error}`);

    return false;
  }
}

async function main() {
  console.log('='.repeat(70));
  console.log('Testing Basic Chat Completion for Various Models');
  console.log(`Router URL: ${baseURL}`);
  console.log('='.repeat(70));

  let passed = 0;
  let failed = 0;

  for (const model of MODELS_TO_TEST) {
    const success = await testModel(model.name, model.displayName);
    if (success) {
      passed++;
    } else {
      failed++;
    }
  }

  console.log('\n' + '='.repeat(70));
  console.log('SUMMARY');
  console.log('='.repeat(70));
  console.log(`Passed: ${passed} / ${MODELS_TO_TEST.length}`);
  console.log(`Failed: ${failed} / ${MODELS_TO_TEST.length}`);
  console.log('='.repeat(70));
}

main();
