import i18n from 'i18next'
import { initReactI18next } from 'react-i18next'
import LanguageDetector from 'i18next-browser-languagedetector'

import en from './locales/en.json'
import zhTW from './locales/zh-TW.json'
import zhCN from './locales/zh-CN.json'
import ja from './locales/ja.json'
import de from './locales/de.json'
import ko from './locales/ko.json'

export const SUPPORTED_LANGUAGES = [
  { code: 'zh-TW', label: '繁體中文' },
  { code: 'zh-CN', label: '简体中文' },
  { code: 'en',    label: 'English' },
  { code: 'ja',    label: '日本語' },
  { code: 'de',    label: 'Deutsch' },
  { code: 'ko',    label: '한국어' },
]

i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources: {
      en:    { translation: en },
      'zh-TW': { translation: zhTW },
      'zh-CN': { translation: zhCN },
      ja:    { translation: ja },
      de:    { translation: de },
      ko:    { translation: ko },
    },
    fallbackLng: 'zh-TW',
    detection: {
      order: ['localStorage', 'navigator'],
      caches: ['localStorage'],
      lookupLocalStorage: 'notetreelm_language',
    },
    interpolation: { escapeValue: false },
  })

export default i18n
