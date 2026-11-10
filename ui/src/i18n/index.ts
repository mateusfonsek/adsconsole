import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import pt from "./pt.json";
import en from "./en.json";

export type Lang = "pt" | "en";

/** System language: any Portuguese becomes `pt`; everything else, `en`. */
export function systemLang(): Lang {
  return navigator.language.toLowerCase().startsWith("pt") ? "pt" : "en";
}

export function createI18n(lng: Lang = systemLang()) {
  const instance = i18n.createInstance();
  void instance.use(initReactI18next).init({
    resources: { pt: { translation: pt }, en: { translation: en } },
    lng,
    fallbackLng: "en",
    interpolation: { escapeValue: false },
    initAsync: false,
  });
  return instance;
}
