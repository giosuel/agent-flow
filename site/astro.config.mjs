// AgentFlow's website: a custom landing page (src/pages/index.astro) and the docs (Starlight).
//
// SITE_URL and SITE_BASE decide where it's served. They default to the GitHub project page,
// https://giosuel.github.io/agent-flow/; the Pages workflow passes what GitHub reports, so a
// custom domain later needs no change here.
import { defineConfig, passthroughImageService } from 'astro/config';
import starlight from '@astrojs/starlight';
import { satteri } from '@astrojs/markdown-satteri';
import codeTheme from './src/code-theme.mjs';

const site = process.env.SITE_URL || 'https://giosuel.github.io';
// An empty SITE_BASE (a custom domain) means the root.
const base = process.env.SITE_BASE === undefined ? '/agent-flow' : process.env.SITE_BASE || '/';

/** Links in the docs' Markdown are written from the site's root ("/docs/concepts/"); this puts
 * the base in front of them, so they work on any base. */
const prefix = base.replace(/\/$/, '');
const baseLinks = {
  name: 'agentflow-base-links',
  link(node, ctx) {
    const url = node.url;
    if (prefix && url.startsWith('/') && !url.startsWith('//') && !url.startsWith(prefix + '/')) {
      ctx.setProperty(node, 'url', prefix + url);
    }
  },
};

export default defineConfig({
  site,
  base,
  trailingSlash: 'always',
  // No raster images to optimise: everything is CSS and SVG.
  image: { service: passthroughImageService() },
  markdown: { processor: satteri({ mdastPlugins: [baseLinks] }) },
  vite: {
    // The landing page and the docs use the app's own stylesheet and fonts from ../static.
    server: { fs: { allow: ['..'] } },
  },
  integrations: [
    starlight({
      title: 'AgentFlow',
      description:
        'Project management for a team of Claude agents and the person they work for: plans, questions, pull requests, builds and a log, over MCP.',
      favicon: '/favicon.svg',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/giosuel/agent-flow' }],
      editLink: { baseUrl: 'https://github.com/giosuel/agent-flow/edit/main/site/' },
      customCss: ['./src/styles/docs.css'],
      components: {
        SiteTitle: './src/components/starlight/SiteTitle.astro',
        ThemeProvider: './src/components/starlight/ThemeProvider.astro',
        ThemeSelect: './src/components/starlight/ThemeSelect.astro',
        PageTitle: './src/components/starlight/PageTitle.astro',
      },
      expressiveCode: {
        themes: [codeTheme],
        useStarlightUiThemeColors: false,
        styleOverrides: {
          borderRadius: '10px',
          borderColor: '#26262D',
          codeBackground: '#0A0A0C',
          codeFontFamily: '"IBM Plex Mono", ui-monospace, monospace',
          codeFontSize: '0.84rem',
          uiFontFamily: '"IBM Plex Sans", system-ui, sans-serif',
          frames: {
            editorBackground: '#0A0A0C',
            terminalBackground: '#0A0A0C',
            editorTabBarBackground: '#0F0F12',
            terminalTitlebarBackground: '#0F0F12',
            editorActiveTabBackground: '#0A0A0C',
            editorActiveTabIndicatorTopColor: '#A78BFA',
            editorTabBarBorderBottomColor: '#26262D',
            terminalTitlebarBorderBottomColor: '#26262D',
            terminalTitlebarDotsForeground: '#34343D',
            frameBoxShadowCssValue: 'none',
            inlineButtonBorder: '#34343D',
            tooltipSuccessBackground: '#4C3D8A',
          },
        },
      },
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'Introduction', slug: 'docs' },
            { label: 'Getting started', slug: 'docs/getting-started' },
          ],
        },
        {
          label: 'Concepts',
          items: [
            { label: 'Projects', slug: 'docs/concepts/projects' },
            { label: 'Milestones and steps', slug: 'docs/concepts/milestones' },
            { label: 'Tasks', slug: 'docs/concepts/tasks' },
            { label: 'Questions and decisions', slug: 'docs/concepts/questions' },
            { label: 'Pull requests', slug: 'docs/concepts/pull-requests' },
            { label: 'Releases', slug: 'docs/concepts/releases' },
            { label: 'Test builds', slug: 'docs/concepts/builds' },
            { label: 'Notes and screenshots', slug: 'docs/concepts/notes' },
            { label: 'Contributions', slug: 'docs/concepts/contributions' },
            { label: 'The log', slug: 'docs/concepts/log' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'The flow command', slug: 'docs/reference/flow' },
            { label: 'MCP tools', slug: 'docs/reference/mcp' },
            { label: 'Configuration', slug: 'docs/reference/configuration' },
          ],
        },
        {
          label: 'Running it',
          items: [
            { label: 'Self-hosting', slug: 'docs/self-hosting' },
            { label: 'Build servers', slug: 'docs/build-servers' },
            { label: 'Development', slug: 'docs/development' },
          ],
        },
      ],
    }),
  ],
});
