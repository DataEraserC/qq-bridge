// VitePress 默认主题 + mermaid 渲染插件。
// config.mjs 里只写了 `withMermaid` 的配置；```mermaid 代码块要真的画成图，
// 还得在客户端把这个插件挂上（这是 vitepress-plugin-mermaid 的要求）。
import DefaultTheme from 'vitepress/theme'
import MermaidPlugin from 'vitepress-plugin-mermaid/MermaidPlugin'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.use(MermaidPlugin)
  },
}
