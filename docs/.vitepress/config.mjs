import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { withMermaid } from 'vitepress-plugin-mermaid'

const SRC = path.dirname(fileURLToPath(import.meta.url)) // docs/.vitepress
const DOCS = path.resolve(SRC, '..')

// ADR 侧边栏：直接从 docs/adr/ 目录生成，标题取每个文件的一级标题。
// 新增 ADR 不用改这里，加个文件就有。
const adrItems = fs
  .readdirSync(path.join(DOCS, 'adr'))
  .filter((f) => f.endsWith('.md'))
  .sort()
  .map((f) => {
    const raw = fs.readFileSync(path.join(DOCS, 'adr', f), 'utf8')
    const line = raw.split('\n').find((l) => l.startsWith('# '))
    const title = line ? line.replace(/^#\s*/, '').trim() : f.replace(/\.md$/, '')
    return { text: title, link: `/adr/${f.replace(/\.md$/, '')}` }
  })

const coreItems = [
  { text: '愿景', link: '/01-vision' },
  { text: '术语表', link: '/02-glossary' },
  { text: '架构', link: '/03-architecture' },
  { text: '协议', link: '/04-protocol' },
  { text: 'harness 适配器', link: '/06-harness-adapters' },
  { text: '存储', link: '/07-storage' },
]

const pluginItems = [
  { text: '插件开发指南', link: '/08-plugin-development' },
  { text: '平台适配器', link: '/05-adapters' },
]

export default withMermaid({
  // mermaid 画图配置：mermaidPlugin.class 是渲染容器的 CSS 类；
  // 暗色主题由插件自动跟随（body 类名带 dark 就切 dark 画图主题）。
  mermaidPlugin: { class: 'mermaid' },
  base: '/qq-bridge/',
  lang: 'zh-CN',
  title: 'acb-bridge',
  description: '把 AI agent 接进聊天平台的桥 —— 多 harness × 多平台',
  ignoreDeadLinks: true,
  themeConfig: {
    nav: [
      { text: '首页', link: '/' },
      { text: '使用指南', link: '/guide/', activeMatch: '/guide/' },
      {
        text: 'ACB 开发者',
        link: '/03-architecture',
        activeMatch:
          '/(01-vision|02-glossary|03-architecture|04-protocol|06-harness-adapters|07-storage)/',
      },
      {
        text: '插件开发者',
        link: '/08-plugin-development',
        activeMatch: '/(05-adapters|08-plugin-development)/',
      },
      { text: 'ADR', link: '/adr/0001-rust-workspace', activeMatch: '/adr/' },
      { text: 'GitHub', link: 'https://github.com/DataEraserC/qq-bridge' },
    ],
    sidebar: {
      // 用户文档：说人话，按使用顺序排。
      '/guide/': [
        {
          text: '使用指南',
          items: [
            { text: 'acb-bridge 是什么', link: '/guide/' },
            { text: '快速开始', link: '/guide/getting-started' },
            { text: '配置手册', link: '/guide/config' },
            { text: '网页控制台', link: '/guide/console' },
            { text: '日常使用', link: '/guide/talking' },
            { text: '排障手册', link: '/guide/troubleshooting' },
          ],
        },
      ],
      // 开发者文档：先分「ACB 开发者」和「插件开发者」，再是 ADR。
      '/': [
        {
          text: 'ACB 开发者',
          items: coreItems,
        },
        {
          text: '插件开发者',
          items: pluginItems,
        },
        {
          text: '架构决策记录（ADR）',
          collapsible: true,
          items: adrItems,
        },
      ],
    },
    socialLinks: [{ icon: 'github', link: 'https://github.com/DataEraserC/qq-bridge' }],
    search: { provider: 'local' },
    outline: { level: [2, 3], label: '本页目录' },
    docFooter: { prev: '上一篇', next: '下一篇' },
    lastUpdated: { text: '最后更新' },
    returnToTopLabel: '回到顶部',
    sidebarMenuLabel: '目录',
  },
})
