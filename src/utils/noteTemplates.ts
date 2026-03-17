export const NOTE_TEMPLATES = [
  {
    id: 'concept',
    label: '概念定義',
    content: (title: string) =>
      `---\nstatus: draft\ntags: [concept]\n---\n\n# ${title}\n\n## 定義\n\n> \n\n## 詳細說明\n\n\n\n## 相關概念\n\n- \n\n## 來源\n\n- \n`,
  },
  {
    id: 'procedure',
    label: '操作步驟',
    content: (title: string) =>
      `---\nstatus: draft\ntags: [procedure]\n---\n\n# ${title}\n\n## 前置條件\n\n- \n\n## 步驟\n\n1. \n2. \n3. \n\n## 注意事項\n\n- \n`,
  },
  {
    id: 'reference',
    label: '參考資料',
    content: (title: string) =>
      `---\nstatus: draft\ntags: [reference]\n---\n\n# ${title}\n\n## 摘要\n\n\n\n## 重點整理\n\n- \n\n## 原始連結\n\n- \n`,
  },
] as const

export type TemplateId = typeof NOTE_TEMPLATES[number]['id']
