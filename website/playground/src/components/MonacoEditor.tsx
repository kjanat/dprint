import type * as monacoEditorForTypes from "monaco-editor";
import { useEffect, useRef } from "react";

export interface MonacoEditorProps {
  monaco: typeof monacoEditorForTypes;
  value: string;
  language: string | undefined;
  theme: string;
  options: monacoEditorForTypes.editor.IStandaloneEditorConstructionOptions;
  onChange: (value: string) => void;
  editorDidMount: (editor: monacoEditorForTypes.editor.IStandaloneCodeEditor) => void;
}

export function MonacoEditor({ monaco, value, language, theme, options, onChange, editorDidMount }: MonacoEditorProps) {
  const container = useRef<HTMLDivElement>(null);
  const editor = useRef<monacoEditorForTypes.editor.IStandaloneCodeEditor>(null);
  const changeHandler = useRef(onChange);
  changeHandler.current = onChange;
  const reported = useRef(value);

  useEffect(() => {
    const element = container.current;
    if (element == null) return;
    const instance = monaco.editor.create(element, { ...options, value, language, theme });
    editor.current = instance;
    const subscription = instance.onDidChangeModelContent(() => {
      const text = instance.getValue();
      reported.current = text;
      changeHandler.current(text);
    });
    editorDidMount(instance);
    return () => {
      subscription.dispose();
      instance.dispose();
      editor.current = null;
    };
  }, [monaco]);

  useEffect(() => {
    const instance = editor.current;
    if (instance == null || value === reported.current) return;
    reported.current = value;
    instance.setValue(value);
  }, [value]);

  useEffect(() => {
    const model = editor.current?.getModel();
    if (model != null && language != null) monaco.editor.setModelLanguage(model, language);
  }, [language]);

  useEffect(() => {
    monaco.editor.setTheme(theme);
  }, [theme]);

  useEffect(() => {
    editor.current?.updateOptions(options);
  }, [options]);

  return <div ref={container} style={{ width: "100%", height: "100%" }} />;
}
