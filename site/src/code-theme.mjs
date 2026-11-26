// Code blocks in AgentFlow's colours (the tokens of static/ui.css): a small VS Code theme for
// Expressive Code.
export default {
  name: 'agentflow',
  type: 'dark',
  colors: {
    'editor.background': '#0A0A0C',
    'editor.foreground': '#C6C6CE',
    'editor.selectionBackground': '#251B40',
    'terminal.ansiBrightBlack': '#55555F',
  },
  tokenColors: [
    { scope: ['comment', 'punctuation.definition.comment'], settings: { foreground: '#6E6E7A', fontStyle: 'italic' } },
    { scope: ['keyword', 'storage', 'storage.type', 'keyword.control'], settings: { foreground: '#C4B5FD' } },
    { scope: ['string', 'string.quoted', 'markup.inline.raw'], settings: { foreground: '#86EFAC' } },
    { scope: ['constant.numeric', 'constant.language', 'constant.other'], settings: { foreground: '#FDBA74' } },
    { scope: ['entity.name.function', 'support.function', 'meta.function-call'], settings: { foreground: '#9ED8FF' } },
    { scope: ['variable.parameter', 'variable.other.readwrite', 'variable'], settings: { foreground: '#EDEDF0' } },
    { scope: ['entity.name.tag', 'support.type.property-name', 'entity.name.type', 'support.type'], settings: { foreground: '#A78BFA' } },
    { scope: ['entity.other.attribute-name', 'meta.object-literal.key'], settings: { foreground: '#D2C4FF' } },
    { scope: ['punctuation', 'meta.brace', 'keyword.operator'], settings: { foreground: '#8E8E9A' } },
    { scope: ['entity.name.section', 'markup.heading'], settings: { foreground: '#EDEDF0', fontStyle: 'bold' } },
    { scope: ['support.variable', 'variable.other.constant', 'variable.language'], settings: { foreground: '#86E3C8' } },
  ],
};
