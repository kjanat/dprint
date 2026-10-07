import type { FileMatchingInfo, PluginInfo } from "@dprint/formatter";
import JSON5 from "json5";
import type React from "react";
import { type ChangeEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { setupTheme } from "../../src/scripts/theme.js";
import { CodeEditor, Spinner } from "./components/index.ts";
import * as formatterWorker from "./FormatterWorker.ts";
import { getLanguageFromPluginUrl, getPluginShortNameFromPluginUrl } from "./plugins/index.ts";

import "./Playground.css";

export interface PlaygroundProps {
  configText: string;
  onConfigTextChanged: (text: string) => void;
  text: string;
  onTextChanged: (text: string) => void;
  fileExtension: string;
  onFileExtensionChanged: (ext: string) => void;
  formattedText: string;
  selectedPluginInfo: PluginInfo;
  fileMatchingInfo: FileMatchingInfo;
  selectedPluginUrl: string;
  pluginUrls: string[];
  onSelectPluginUrl: (pluginUrl: string) => void;
  isLoading: boolean;
}

export function Playground({
  configText,
  onConfigTextChanged,
  text,
  onTextChanged,
  fileExtension,
  onFileExtensionChanged,
  formattedText,
  fileMatchingInfo,
  selectedPluginUrl,
  selectedPluginInfo,
  pluginUrls,
  onSelectPluginUrl,
  isLoading,
}: PlaygroundProps) {
  const [scrollTop, setScrollTop] = useState(0);
  useEffect(setupTheme, []);

  useEffect(() => {
    if (fileMatchingInfo.fileExtensions.length > 0) {
      if (
        fileExtension == null
        || !fileMatchingInfo.fileExtensions.includes(fileExtension)
      ) {
        onFileExtensionChanged(fileMatchingInfo.fileExtensions[0]);
      }
    }
  }, [fileMatchingInfo, fileExtension, onFileExtensionChanged]);

  useEffect(() => {
    const timeout = setTimeout(() => {
      formatterWorker.formatText(`file.${fileExtension ?? "ts"}`, text);
    }, 250);

    return () => clearTimeout(timeout);
  }, [fileExtension, text]);

  useEffect(() => {
    const timeout = setTimeout(() => {
      let config: Record<string, unknown> = {};
      try {
        config = JSON5.parse(configText);
        if (config.lineWidth == null) {
          config.lineWidth = 80;
        }
        formatterWorker.setConfig(config);
      } catch (_) {
        // ignore for now
      }
    }, 250);

    return () => clearTimeout(timeout);
  }, [configText]);

  const lineWidth = useMemo(() => {
    try {
      const lineWidth = parseInt(JSON5.parse(configText).lineWidth, 10);
      if (!Number.isNaN(lineWidth)) return lineWidth;
    } catch (_) {
      // ignore
    }
    return 80;
  }, [configText]);
  const onFileExtensionChange = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      onFileExtensionChanged(event.target.value);
    },
    [onFileExtensionChanged],
  );

  const editorLanguage = getLanguageFromPluginUrl(selectedPluginUrl) ?? "plaintext";
  const langLabel = getPluginShortNameFromPluginUrl(selectedPluginUrl) ?? "plugin";

  // draggable dividers: leftFrac is the left column's width as a fraction of the
  // panes row; inputFrac is the input pane's height as a fraction of the left column.
  const panesRef = useRef<HTMLDivElement>(null);
  const leftColRef = useRef<HTMLDivElement>(null);
  const [leftFrac, setLeftFrac] = useState(0.5);
  const [inputFrac, setInputFrac] = useState(0.65);

  const startColumnResize = useDividerDrag(panesRef, "x", setLeftFrac);
  const startRowResize = useDividerDrag(leftColRef, "y", setInputFrac);

  const onSelectPlugin = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      if (event.target.selectedIndex >= pluginUrls.length) {
        const url = prompt("Please provide a Wasm plugin url:", "");
        if (url != null && url.trim().length > 0) onSelectPluginUrl(url);
        else event.preventDefault();
      } else onSelectPluginUrl(pluginUrls[event.target.selectedIndex]);
    },
    [pluginUrls, onSelectPluginUrl],
  );

  return (
    <div id="App">
      <nav id="AppNav">
        <div className="navInner">
          <a className="brand" href="/">
            dprint
          </a>
          <div className="navLinks">
            <a href="/overview">Overview</a>
            <a className="active" href="/playground">
              Playground
            </a>
            <a href="/sponsor">Sponsor</a>
            <a
              className="ghButton"
              href="https://github.com/kjanat/dprint"
              rel="noopener noreferrer"
            >
              GitHub <span className="ghArrow">↗</span>
            </a>
            <button
              className="theme-toggle"
              type="button"
              data-theme-toggle
              hidden
            >
              <svg viewBox="0 0 20 20" aria-hidden="true">
                <circle
                  cx="10"
                  cy="10"
                  r="7"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.5"
                />
                <path d="M10 3a7 7 0 0 1 0 14Z" fill="currentColor" />
              </svg>
              <span data-theme-label>Theme</span>
            </button>
          </div>
        </div>
      </nav>

      <div className="page">
        <div className="toolbar">
          <div className="toolbarGroup">
            <span className="toolbarLabel">plugin</span>
            <select
              className="control"
              value={selectedPluginUrl}
              onChange={onSelectPlugin}
            >
              {pluginUrls.map((pluginUrl) => (
                <option key={pluginUrl} value={pluginUrl}>
                  {pluginUrl}
                </option>
              ))}
              <option key="custom">Custom…</option>
            </select>
            <select
              className="control"
              value={fileExtension}
              onChange={onFileExtensionChange}
            >
              {fileMatchingInfo.fileExtensions.map((ext) => (
                <option key={ext} value={ext}>
                  .{ext}
                </option>
              ))}
            </select>
          </div>
          <div className="toolbarGroup">
            <button type="button" className="btn btnGhost" onClick={() => onTextChanged("")}>
              Reset
            </button>
            <button
              type="button"
              className="btn btnPrimary"
              onClick={() =>
                formatterWorker.formatText(
                  `file.${fileExtension ?? "ts"}`,
                  text,
                )}
            >
              Format <span className="btnArrow">▸</span>
            </button>
          </div>
        </div>

        <div
          className="panes"
          ref={panesRef}
          style={{
            "--left-frac": leftFrac,
            "--input-frac": inputFrac,
          } as React.CSSProperties}
        >
          <div className="leftCol" ref={leftColRef}>
            <section className="pane inputPane">
              <div className="paneHeader">
                <span className="paneLabel">Input</span>
                <span className="paneMeta">{langLabel}</span>
              </div>
              <div className="paneBody">
                <CodeEditor
                  language={editorLanguage}
                  onChange={onTextChanged}
                  text={text}
                  lineWidth={lineWidth}
                  onScrollTopChange={setScrollTop}
                  scrollTop={scrollTop}
                />
              </div>
            </section>

            <hr
              className="divider dividerHorizontal"
              aria-orientation="horizontal"
              onPointerDown={startRowResize}
            />

            <section className="pane configPane">
              <div className="paneHeader">
                <span className="paneLabel">Config</span>
                <span className="paneMeta">json</span>
              </div>
              <div className="paneBody">
                <CodeEditor
                  language="json"
                  onChange={onConfigTextChanged}
                  jsonSchemaUrl={selectedPluginInfo?.configSchemaUrl}
                  text={configText}
                />
              </div>
            </section>
          </div>

          <hr
            className="divider dividerVertical"
            aria-orientation="vertical"
            onPointerDown={startColumnResize}
          />

          <section className="pane outputPane">
            <div className="paneHeader">
              <span className="paneLabel">Output</span>
              <span
                className="paneMeta"
                style={{
                  color: isLoading ? "var(--text-muted)" : "var(--success)",
                }}
              >
                {isLoading ? "loading…" : "formatted ✓"}
              </span>
            </div>
            <div className="paneBody">
              {isLoading ? <Spinner backgroundColor="var(--code-bg)" /> : (
                <CodeEditor
                  language={editorLanguage}
                  text={formattedText}
                  readonly
                  lineWidth={lineWidth}
                  onScrollTopChange={setScrollTop}
                  scrollTop={scrollTop}
                />
              )}
            </div>
          </section>
        </div>
      </div>
    </div>
  );
}

// returns a pointer-down handler that resizes panes by dragging a divider,
// updating `setFraction` with the pointer position as a fraction (0.15–0.85)
// of the container along the given axis.
function useDividerDrag(
  containerRef: React.RefObject<HTMLElement | null>,
  axis: "x" | "y",
  setFraction: (fraction: number) => void,
) {
  return useCallback(
    (event: React.PointerEvent) => {
      event.preventDefault();
      const container = containerRef.current;
      if (container == null) {
        return;
      }

      const onMove = (moveEvent: PointerEvent) => {
        const rect = container.getBoundingClientRect();
        const fraction = axis === "x"
          ? (moveEvent.clientX - rect.left) / rect.width
          : (moveEvent.clientY - rect.top) / rect.height;
        setFraction(Math.min(0.85, Math.max(0.15, fraction)));
      };

      function onUp() {
        globalThis.removeEventListener("pointermove", onMove);
        globalThis.removeEventListener("pointerup", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      }

      document.body.style.cursor = axis === "x" ? "col-resize" : "row-resize";
      document.body.style.userSelect = "none";
      globalThis.addEventListener("pointermove", onMove);
      globalThis.addEventListener("pointerup", onUp);
    },
    [containerRef, axis, setFraction],
  );
}
