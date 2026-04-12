import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./locales/en";
import zhCN from "./locales/zh-CN";

const STORAGE_KEY = "opencrab-language";

const savedLang = localStorage.getItem(STORAGE_KEY);
const systemLang = typeof navigator !== "undefined" ? navigator.language : "en";
const defaultLang = savedLang ?? (systemLang.startsWith("zh") ? "zh-CN" : "en");

void i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    "zh-CN": { translation: zhCN },
  },
  lng: defaultLang,
  fallbackLng: "en",
  interpolation: {
    escapeValue: false,
  },
});

i18n.on("languageChanged", (lng) => {
  localStorage.setItem(STORAGE_KEY, lng);
});

export default i18n;
