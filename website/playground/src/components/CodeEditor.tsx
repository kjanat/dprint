import type * as monacoEditorForTypes from "monaco-editor";
import React from "react";
import type ReactMonacoEditorForTypes from "react-monaco-editor";
import { Spinner } from "./Spinner";
import { getTheme } from "../../../src/scripts/theme.js";

export interface CodeEditorProps {
  onChange?: (text: string) => void;
  text?: string;
  readonly?: boolean;
  lineWidth?: number;
  scrollTop?: number;
  jsonSchemaUrl?: string;
  onScrollTopChange?: (scrollTop: number) => void;
  language: "typescript" | "json" | "markdown" | "toml" | "dockerfile" | "plaintext" | "css" | "html" | "yaml" | "php" | undefined;
}

export interface CodeEditorState {
  editorComponent: (typeof ReactMonacoEditorForTypes) | undefined | false;
  theme: "light" | "dark";
}

export class CodeEditor extends React.Component<CodeEditorProps, CodeEditorState> {
  private editor: monacoEditorForTypes.editor.IStandaloneCodeEditor | undefined;
  private monacoEditor: typeof monacoEditorForTypes | undefined;
  private outerContainerRef = React.createRef<HTMLDivElement>();
  private disposables: monacoEditorForTypes.IDisposable[] = [];

  constructor(props: CodeEditorProps) {
    super(props);
    this.state = {
      editorComponent: undefined,
      theme: getTheme(),
    };
    this.editorDidMount = this.editorDidMount.bind(this);

    const reactMonacoEditorPromise = import("react-monaco-editor");
    import("monaco-editor").then(monacoEditor => {
      this.monacoEditor = monacoEditor;
      if (this.props.language === "typescript") {
        monacoEditor.typescript.typescriptDefaults.setCompilerOptions({
          noLib: true,
          target: monacoEditor.typescript.ScriptTarget.ESNext,
          allowNonTsExtensions: true,
        });
        monacoEditor.typescript.typescriptDefaults.setDiagnosticsOptions({
          noSyntaxValidation: true,
          noSemanticValidation: true,
        });
      }

      monacoEditor.editor.defineTheme("dprint-dark", {
        base: "vs-dark",
        inherit: true,
        rules: [],
        colors: {
          "editor.background": "#282f39",
          "editor.foreground": "#dde3ec",
          "editorLineNumber.foreground": "#acbbce",
          "editorRuler.foreground": "#4b5665",
        },
      });
      monacoEditor.editor.defineTheme("dprint-light", {
        base: "vs",
        inherit: true,
        rules: [],
        colors: {
          "editor.background": "#ffffff",
          "editor.foreground": "#354153",
          "editorLineNumber.foreground": "#56677d",
          "editorRuler.foreground": "#d3dce7",
        },
      });

      reactMonacoEditorPromise.then(editor => {
        this.setState({ editorComponent: editor.default });
      }).catch(err => {
        console.error(err);
        this.setState({ editorComponent: false });
      });
    }).catch(err => {
      console.error(err);
      this.setState({ editorComponent: false });
    });
  }

  render() {
    this.updateScrollTop();
    this.updateJsonSchema();

    return (
      <div className="codeEditor" ref={this.outerContainerRef}>
        {this.getEditor()}
      </div>
    );
  }

  componentDidMount() {
    window.addEventListener("dprint:theme-change", this.onThemeChange);
    this.onThemeChange();
  }

  private onThemeChange = () => this.setState({ theme: getTheme() });

  componentWillUnmount() {
    window.removeEventListener("dprint:theme-change", this.onThemeChange);
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.disposables.length = 0; // clear
  }

  private getEditor() {
    if (this.state.editorComponent == null) {
      return <Spinner backgroundColor="var(--code-bg)" />;
    }
    if (this.state.editorComponent === false) {
      return <div className="errorMessage">Error loading code editor. Please refresh the page to try again.</div>;
    }

    return (
      <this.state.editorComponent
        width="100%"
        height="100%"
        value={this.props.text}
        theme={`dprint-${this.state.theme}`}
        language={this.props.language}
        onChange={text => this.props.onChange && this.props.onChange(text)}
        editorDidMount={this.editorDidMount}
        options={{
          automaticLayout: false,
          renderWhitespace: "all",
          readOnly: this.props.readonly || false,
          minimap: { enabled: false },
          quickSuggestions: false,
          rulers: this.props.lineWidth == null ? [] : [this.props.lineWidth],
        }}
      />
    );
  }

  private editorDidMount(editor: monacoEditorForTypes.editor.IStandaloneCodeEditor) {
    this.editor = editor;

    this.disposables.push(this.editor.onDidChangeModelContent(() => {
      if (this.props.readonly) {
        this.editor!.setPosition({
          column: 1,
          lineNumber: 1,
        });
      }
    }));

    this.disposables.push(this.editor.onDidScrollChange(e => {
      if (e.scrollTopChanged && this.props.onScrollTopChange) {
        this.props.onScrollTopChange(e.scrollTop);
      }
    }));

    // manually refresh the layout of the editor (lightweight compared to monaco editor)
    let lastHeight = 0;
    let lastWidth = 0;
    const intervalId = setInterval(() => {
      const containerElement = this.outerContainerRef.current;
      if (containerElement == null) {
        return;
      }

      const width = containerElement.offsetWidth;
      const height = containerElement.offsetHeight;
      if (lastHeight === height && lastWidth === width) {
        return;
      }

      editor.layout();

      lastHeight = height;
      lastWidth = width;
    }, 500);
    this.disposables.push({ dispose: () => clearInterval(intervalId) });
  }

  private lastScrollTop = 0;
  private updateScrollTop() {
    if (this.editor == null || this.lastScrollTop === this.props.scrollTop) {
      return;
    }

    // todo: not sure how to not do this in the render method? I'm not a react/web person.
    setTimeout(() => {
      if (this.props.scrollTop != null) {
        this.editor!.setScrollTop(this.props.scrollTop);
        this.lastScrollTop = this.props.scrollTop;
      }
    }, 0);
  }

  private updateJsonSchema() {
    if (this.monacoEditor != null && this.props.jsonSchemaUrl != null) {
      if (this.monacoEditor.json.jsonDefaults.diagnosticsOptions.schemas?.[0]?.uri !== this.props.jsonSchemaUrl) {
        this.monacoEditor.json.jsonDefaults.setDiagnosticsOptions({
          validate: true,
          allowComments: true,
          enableSchemaRequest: true,
          schemas: [{
            uri: this.props.jsonSchemaUrl,
            fileMatch: ["*"],
          }],
        });
      }
    }
  }
}
