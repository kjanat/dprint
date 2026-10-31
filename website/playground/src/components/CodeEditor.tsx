import type * as monacoEditorForTypes from "monaco-editor";
import { Component, createRef } from "react";
import { getTheme } from "../../../src/scripts/theme.ts";
import { MonacoEditor } from "./MonacoEditor.tsx";
import { Spinner } from "./Spinner.tsx";
import "../monacoWorkers.ts";

export interface CodeEditorProps {
  onChange?: (text: string) => void;
  text?: string;
  readonly?: boolean;
  lineWidth?: number;
  scrollTop?: number;
  jsonSchemaUrl?: string;
  onScrollTopChange?: (scrollTop: number) => void;
  language:
    | "typescript"
    | "json"
    | "markdown"
    | "toml"
    | "dockerfile"
    | "plaintext"
    | "css"
    | "html"
    | "yaml"
    | "php"
    | undefined;
}

export interface CodeEditorState {
  monaco: typeof monacoEditorForTypes | undefined | false;
  theme: "light" | "dark";
}

export class CodeEditor extends Component<
  CodeEditorProps,
  CodeEditorState
> {
  private editor: monacoEditorForTypes.editor.IStandaloneCodeEditor | undefined;
  private outerContainerRef = createRef<HTMLDivElement>();
  private disposables: monacoEditorForTypes.IDisposable[] = [];

  constructor(props: CodeEditorProps) {
    super(props);
    this.state = {
      monaco: undefined,
      theme: getTheme(),
    };
    this.editorDidMount = this.editorDidMount.bind(this);

    import("monaco-editor")
      .then((monaco) => {
        if (this.props.language === "typescript") {
          monaco.typescript.typescriptDefaults.setCompilerOptions({
            noLib: true,
            target: monaco.typescript.ScriptTarget.ESNext,
            allowNonTsExtensions: true,
          });
          monaco.typescript.typescriptDefaults.setDiagnosticsOptions({
            noSyntaxValidation: true,
            noSemanticValidation: true,
          });
        }

        monaco.editor.defineTheme("dprint-dark", {
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
        monaco.editor.defineTheme("dprint-light", {
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

        this.setState({ monaco });
      })
      .catch((err) => {
        console.error(err);
        this.setState({ monaco: false });
      });
  }

  override render() {
    this.updateScrollTop();
    this.updateJsonSchema();

    return (
      <div className="codeEditor" ref={this.outerContainerRef}>
        {this.getEditor()}
      </div>
    );
  }

  override componentDidMount() {
    globalThis.addEventListener("dprint:theme-change", this.onThemeChange);
    this.onThemeChange();
  }

  private onThemeChange = () => this.setState({ theme: getTheme() });

  override componentWillUnmount() {
    globalThis.removeEventListener("dprint:theme-change", this.onThemeChange);
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.disposables.length = 0; // clear
  }

  private getEditor() {
    if (this.state.monaco == null) {
      return <Spinner backgroundColor="var(--code-bg)" />;
    }
    if (this.state.monaco === false) {
      return (
        <div className="errorMessage">
          Error loading code editor. Please refresh the page to try again.
        </div>
      );
    }

    return (
      <MonacoEditor
        monaco={this.state.monaco}
        value={this.props.text ?? ""}
        theme={`dprint-${this.state.theme}`}
        language={this.props.language}
        onChange={(text) => this.props.onChange?.(text)}
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

  private editorDidMount(
    editor: monacoEditorForTypes.editor.IStandaloneCodeEditor,
  ) {
    this.editor = editor;

    this.disposables.push(
      this.editor.onDidChangeModelContent(() => {
        if (this.props.readonly) {
          editor.setPosition({
            column: 1,
            lineNumber: 1,
          });
        }
      }),
    );

    this.disposables.push(
      this.editor.onDidScrollChange((e) => {
        if (e.scrollTopChanged && this.props.onScrollTopChange) {
          this.props.onScrollTopChange(e.scrollTop);
        }
      }),
    );

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
      const editor = this.editor;
      const scrollTop = this.props.scrollTop;
      if (editor != null && scrollTop != null) {
        editor.setScrollTop(scrollTop);
        this.lastScrollTop = scrollTop;
      }
    }, 0);
  }

  private updateJsonSchema() {
    const monaco = this.state.monaco;
    if (monaco && this.props.jsonSchemaUrl != null) {
      if (
        monaco.json.jsonDefaults.diagnosticsOptions.schemas?.[0]
          ?.uri !== this.props.jsonSchemaUrl
      ) {
        monaco.json.jsonDefaults.setDiagnosticsOptions({
          validate: true,
          allowComments: true,
          enableSchemaRequest: true,
          schemas: [
            {
              uri: this.props.jsonSchemaUrl,
              fileMatch: ["*"],
            },
          ],
        });
      }
    }
  }
}
