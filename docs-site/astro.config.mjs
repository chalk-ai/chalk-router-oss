import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  integrations: [starlight({
    title: 'Chalk Router API',
    description: 'Source-derived API reference and integration guides for Chalk Router.',
    social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/chalk-ai/chalk-router-oss' }],
    customCss: ['./src/styles/custom.css'],
    sidebar: [
      { label: 'Start here', items: ['index', 'guides/quickstart', 'guides/authentication'] },
      { label: 'Inference', items: ['guides/chat', 'guides/responses', 'guides/provider-apis', 'guides/media', 'guides/decisions', 'guides/models', 'guides/errors-streaming'] },
      { label: 'Administration', items: ['guides/admin', 'guides/operations'] },
      { label: 'Generated reference', collapsed: true, items: [{ autogenerate: { directory: 'reference' } }] },
      'maintenance',
    ],
  })],
});
