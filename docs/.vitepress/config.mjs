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

const devItems = [
  { text: '愿景', link: '/01-vision' },
  { text: '术语表', link: '/02-glossary' },
  { text: '架构', link: '/03-architecture' },
  { text: '协议', link: '/04-protocol' },
  { text: '平台适配器', link: '/05-adapters' },
  { text: 'harness 适配器', link: '/06-harness-adapters' },
  { text: '存储', link: '/07-storage' },
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
      { text: '开发者文档', link: '/03-architecture', activeMatch: '/(?!guide/)' },
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
      // 开发者文档：架构 + ADR，写给改代码的人。
      '/': [
        {
          text: '开发者文档',
          items: devItems,
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
