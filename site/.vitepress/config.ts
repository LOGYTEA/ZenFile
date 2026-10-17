import { defineConfig } from 'vitepress'

export default defineConfig({
  lang: 'zh-CN',
  title: 'ZenFile',
  description: '原生 Win32 文件管理器',
  // 站点实际部署在用户主页仓库的 zenfile/ 子目录：https://logytea.github.io/zenfile/
  // 不是 ZenFile 仓库的项目页（base 不能写 /ZenFile/）。
  // 注意：vitepress preview 在根路径 serve，base 写错本地测不出来，
  // 部署后才会全站 404。改 base 后务必在子路径下用静态服务器实测。
  base: '/zenfile/',
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
