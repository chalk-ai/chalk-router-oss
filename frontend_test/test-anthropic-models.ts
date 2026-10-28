import OpenAI from 'openai';
import * as dotenv from 'dotenv';

dotenv.config();

const baseURL = process.env.ROUTER_BASE_URL || 'http://localhost:4001/v1';

const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL,
});

// Try various Anthropic model name patterns
const SONNET_VARIANTS = [
  'claude-3-5-sonnet-20241022',
  'claude-3-5-sonnet-20240620',
  'claude-3-sonnet-20240229',
  'claude-sonnet-3.5',
  'claude-sonnet-4',
  'claude-sonnet-4.5',
  'claude-sonnet-4-20250514',
];

const OPUS_VARIANTS = [
  'claude-3-opus-20240229',
  'claude-3-opus-20250219',
  'claude-opus-3',
  'claude-opus-4',
  'claude-opus-4.5',
  'claude-opus-4-20250514',
];

async function testModel(modelName: string) {
  try {
    const response = await client.chat.completions.create({
      model: `anthropic/${modelName}`,
      messages: [
        { role: 'user', content: 'Say "Hello"' }
      ],
      max_completion_tokens: 20,
    });

    console.log(`✓ anthropic/${modelName} - Model returned: ${response.model}`);
    return true;
  } catch (error: any) {
    console.log(`✗ anthropic/${modelName} - ${error.status || 'Error'}: ${error.message}`);
    return false;
  }
}

async function main() {
  console.log('Testing Sonnet variants:\n');
  for (const model of SONNET_VARIANTS) {
    await testModel(model);
  }

  console.log('\nTesting Opus variants:\n');
  for (const model of OPUS_VARIANTS) {
    await testModel(model);
  }
}

main();
