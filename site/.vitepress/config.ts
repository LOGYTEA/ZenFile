import { defineConfig } from 'vitepress'

export default defineConfig({
  lang: 'zh-CN',
  title: 'ZenFile',
  description: '原生 Win32 文件管理器',
  // 部署到项目页 https://logytea.github.io/ZenFile/ ，不是用户页 logytea.github.io/
  // 本地 preview 也在根路径 serve，所以这个错误本地测不出来 —— 部署后才发现全站 404
  base: '/ZenFile/',
  appearance: 'dark',
  cleanUrls: false,
  lastUpdated: true,

  themeConfig: {
    nav: [
      { text: '开始使用', link: '/guide/install', activeMatch: '/guide/' },
      { text: '开发文档', link: '/docs/new-plugin', activeMatch: '/docs/' },
    ],

    sidebar: [
      {
        text: '开始使用',
        items: [
          { text: '安装', link: '/guide/install' },
          { text: '插件', link: '/guide/plugins' },
        ],
      },
      {
        text: '开发文档',
        items: [
          { text: '开始你的第一个插件', link: '/docs/new-plugin' },
          { text: 'ZenFile API', link: '/docs/api' },
          { text: '架构设计', link: '/docs/architecture' },
          { text: '性能实测', link: '/docs/performance' },
        ],
      },
    ],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/LOGYTEA/ZenFile' },
    ],

    search: { provider: 'local' },

    outline: { level: [2, 3] },
  },
})
