type D3DragEvent<E extends Element, D, S> = import("d3").D3DragEvent<E, D, S>;
type D3ZoomEvent<E extends Element, D> = import("d3").D3ZoomEvent<E, D>;
type SimulationLinkDatum<N extends import("d3").SimulationNodeDatum> = import("d3").SimulationLinkDatum<N>;
type ZoomTransform = import("d3").ZoomTransform;
type CodeViewTextSegment = import("./types.d.ts").CodeViewTextSegment;
type GraphPrintNode = import("./types.d.ts").GraphPrintNode;
type PrintNode = import("./types.d.ts").PrintNode;
type WriterNode = import("./types.d.ts").WriterNode;

interface GraphLink extends SimulationLinkDatum<GraphPrintNode> {
  color: string | undefined;
  originatingNodeId: number | undefined;
}

const d3 = window.d3;
const traceResult = getTransformedTraceResult();
Object.assign(globalThis, { onLoad });

function onLoad() {
  const appData = {
    traceIndex: traceResult.traces.length - 1,
    selectedNodeId: traceResult.traces[traceResult.traces.length - 1].printNodeId,
  };

  const slider = createSlider((value) => {
    appData.traceIndex = value;
    appData.selectedNodeId = traceResult.traces[value].printNodeId;
    refreshApp();
  });
  const codeView = createCodeView();
  const infoArea = createInfoArea();
  const graph = createGraph((printNodeId) => {
    const traceIndex = getNextTraceIndex();
    // might not have had a trace that visited this print node
    if (traceIndex >= 0) {
      appData.traceIndex = traceIndex;
    }
    appData.selectedNodeId = printNodeId;
    refreshApp();

    function getNextTraceIndex() {
      if (appData.selectedNodeId === printNodeId) {
        const traceIndex = traceResult.traces.findIndex(
          (t, index) => index > appData.traceIndex && t.printNodeId === printNodeId,
        );
        if (traceIndex >= 0) {
          return traceIndex;
        }
      }

      return traceResult.traces.findIndex((t) => t.printNodeId === printNodeId);
    }
  });

  const mainElement = document.createElement("div");
  mainElement.id = "main";
  const splitViewElement = document.createElement("div");
  splitViewElement.id = "split-view";
  splitViewElement.appendChild(codeView.element);
  const graphContainer = document.createElement("div");
  graphContainer.id = "graph-container";
  graphContainer.appendChild(graph.element);
  const nodeInfoArea = createNodeInfoArea();
  graphContainer.appendChild(nodeInfoArea.element);
  splitViewElement.appendChild(graphContainer);
  mainElement.appendChild(splitViewElement);
  mainElement.appendChild(infoArea.element);
  mainElement.appendChild(slider.element);
  document.body.appendChild(mainElement);

  refreshApp();

  function refreshApp() {
    slider.setMax(traceResult.traces.length - 1);
    slider.setValue(appData.traceIndex);
    codeView.setTraceIndex(appData.traceIndex);
    infoArea.setTraceIndex(appData.traceIndex);
    graph.setSelectedNodeId(appData.selectedNodeId);
    nodeInfoArea.setSelectedNodeId(appData.selectedNodeId);
  }
}

function getLastNodes(node: PrintNode): PrintNode[] {
  while (node.nextPrintNodeId != null) {
    node = traceResult.getPrintNode(node.nextPrintNodeId);
  }

  const lastNodes: PrintNode[] = [];
  if (node.printItem.kind === "condition") {
    const condition = node.printItem.content;
    if (condition.truePath == null || condition.falsePath == null) {
      lastNodes.push(node);
    }
    if (condition.truePath != null) {
      lastNodes.push(
        ...getLastNodes(traceResult.getPrintNode(condition.truePath)),
      );
    }
    if (condition.falsePath != null) {
      lastNodes.push(
        ...getLastNodes(traceResult.getPrintNode(condition.falsePath)),
      );
    }
  } else if (node.printItem.kind === "rcPath") {
    lastNodes.push(
      ...getLastNodes(traceResult.getPrintNode(node.printItem.content)),
    );
  } else {
    lastNodes.push(node);
  }
  return lastNodes;
}

function getNodeHoverText(node: PrintNode): string {
  const printItem = node.printItem;
  switch (printItem.kind) {
    case "condition":
      return `Condition: ${printItem.content.name} (${node.printNodeId})`;
    case "info":
      return `Info: ${printItem.content.content.name} - ${printItem.content.kind} (${node.printNodeId})`;
    case "rcPath":
      return `RcPath (${node.printNodeId})`;
    case "signal":
      return `Signal: ${printItem.content} (${node.printNodeId})`;
    case "string":
      return `String: ${printItem.content} (${node.printNodeId})`;
    case "anchor":
      return `Anchor: ${printItem.content.name} (${node.printNodeId})`;
    case "conditionReevaluation":
      return `Condition reevaluation: ${printItem.content.name} (${printItem.content.conditionId}) (${node.printNodeId})`;
  }
}

function getNodesAndLinks() {
  const nodes: GraphPrintNode[] = traceResult.printNodes.map((node) => ({
    id: node.printNodeId,
    printNode: node,
    sources: [],
    targets: [],
    depthY: 0,
  }));
  const nodesMap = new Map(nodes.map((n) => [n.printNode.printNodeId, n]));
  const links: GraphLink[] = [];

  for (const node of nodes) {
    const printItem = node.printNode.printItem;
    const printNode = node.printNode;
    if (printItem.kind === "rcPath") {
      const target = getNodeById(printItem.content);
      addLink(node, target);
      addLinksToLastNodes(node, target);
    } else if (printItem.kind === "condition") {
      const condition = printItem.content;
      if (condition.truePath != null) {
        const target = getNodeById(condition.truePath);
        addLink(node, target, "green");
        addLinksToLastNodes(node, target);
      }
      if (condition.falsePath != null) {
        const target = getNodeById(condition.falsePath);
        addLink(node, target, "red");
        addLinksToLastNodes(node, target);
      }
      if (
        (condition.truePath == null || condition.falsePath == null)
        && printNode.nextPrintNodeId != null
      ) {
        addLink(node, getNodeById(printNode.nextPrintNodeId));
      }
    } else if (printNode.nextPrintNodeId != null) {
      addLink(node, getNodeById(printNode.nextPrintNodeId));
    }
  }

  setDepthY(nodes[0]);

  return { nodes, links };

  function setDepthY(firstNode: GraphPrintNode) {
    if (firstNode.sources.length > 0) {
      throw new Error("Must provide the root node.");
    }

    const analyzedNodes = new Set<number>();
    const nodesToAnalyze = [firstNode];

    while (nodesToAnalyze.length > 0) {
      const node = nodesToAnalyze.pop();
      if (node == null) {
        continue;
      }
      node.depthY = node.sources.length === 0
        ? 0
        : Math.max(...node.sources.map((s) => s.depthY)) + 1;
      if (!analyzedNodes.has(node.printNode.printNodeId)) {
        analyzedNodes.add(node.printNode.printNodeId);
        nodesToAnalyze.push(...node.targets);
      }
    }
  }

  function addLinksToLastNodes(source: GraphPrintNode, target: GraphPrintNode) {
    if (source.printNode.nextPrintNodeId != null) {
      const nextPrintNodeTarget = getNodeById(source.printNode.nextPrintNodeId);
      for (const lastNode of getLastNodes(target.printNode)) {
        addLink(
          getNodeById(lastNode.printNodeId),
          nextPrintNodeTarget,
          undefined,
          source.printNode.printNodeId,
        );
      }
    }
  }

  function addLink(source: GraphPrintNode, target: GraphPrintNode, color?: string, originatingNodeId?: number) {
    if (color == null && source.printNode.printItem.kind === "condition") {
      const condition = source.printNode.printItem.content;
      color = condition.truePath == null && condition.falsePath == null
        ? undefined
        : condition.falsePath == null
        ? "red"
        : "green";
    }
    source.targets.push(target);
    target.sources.push(source);
    links.push({
      source: source.id,
      target: target.id,
      color,
      originatingNodeId,
    });
  }

  function getNodeById(id: number): GraphPrintNode {
    const node = nodesMap.get(id);
    if (node == null) {
      throw new Error(`Could not find node: ${id}`);
    }
    return node;
  }
}

function endpoint(end: GraphPrintNode | string | number): GraphPrintNode {
  if (typeof end === "object") return end;
  throw new Error(`Link endpoint ${end} was not resolved to a node.`);
}

function createGraph(onPrintNodeSelect: (printNodeId: number) => void) {
  const { links, nodes } = getNodesAndLinks();

  let wasMouseActivity = false;
  const width = 400;
  const height = 400;
  const simulation = d3
    .forceSimulation(nodes)
    .force(
      "link",
      d3
        .forceLink<GraphPrintNode, GraphLink>(links)
        .id((d) => d.id)
        .distance(10),
    )
    .force("charge", d3.forceManyBody().strength(-3000))
    .force(
      "y",
      d3.forceY<GraphPrintNode>().y((d) => d.depthY * 125),
    );
  const svg = d3
    .create("svg")
    .attr("viewBox", `0 0 ${width} ${height}`)
    .style("font", "40px sans-serif")
    .on("wheel", () => (wasMouseActivity = true))
    .on("click", () => (wasMouseActivity = true));

  const arrow = svg
    .append("svg:defs")
    .selectAll("marker")
    .data(["end"])
    .enter()
    .append("svg:marker")
    .attr("id", String)
    .attr("orient", "auto");
  const arrowInnerPath = arrow.append("svg:path").attr("fill", "#000");

  const drag = d3.drag<SVGGElement, GraphPrintNode>();
  drag.on(
    "drag",
    function(this: SVGGElement, event: D3DragEvent<SVGGElement, GraphPrintNode, GraphPrintNode>, d: GraphPrintNode) {
      d.x = event.x;
      d.y = event.y;
      d3.select(this).raise().attr("transform", `translate(${d.x}, ${d.y})`);
      refreshLinks();
    },
  );

  const nodeRadius = 15;
  const linkThickness = 5;
  const linkG = svg.append("g");
  const link = linkG
    .selectAll<SVGLineElement, GraphLink>("line")
    .data(links)
    .join("line")
    .attr("stroke-opacity", 0.6)
    .attr("stroke", (d) => getLineColor(d))
    .attr(
      "data-originating-node-id",
      (d) => d.originatingNodeId ?? null,
    )
    .style("stroke-width", linkThickness)
    .attr("marker-end", "url(#end)")
    .on(
      "click",
      (_, d) => {
        const originatingNodeId = d.originatingNodeId;
        if (originatingNodeId != null) {
          onPrintNodeSelect(originatingNodeId);
        }
      },
    );
  link.append("title").text(
    (d) => {
      if (d.originatingNodeId != null) {
        return getNodeHoverText(traceResult.getPrintNode(d.originatingNodeId));
      }
      return null;
    },
  );

  const nodeG = svg.append("g");
  const nodeGInner = nodeG
    .append("g")
    .selectAll<SVGGElement, GraphPrintNode>("g")
    .data(nodes)
    .join("g")
    .call(drag);
  const nodeCircle = nodeGInner
    .append("circle")
    .attr("r", nodeRadius)
    .attr("fill", (d) => getNodeColor(d.printNode))
    .attr("stroke", "#000")
    .attr("id", (d) => `node${d.id}`)
    .on(
      "click",
      (_, d) => {
        onPrintNodeSelect(d.id);
      },
    );
  nodeGInner
    .append("text")
    .attr("x", 50)
    .attr("y", "0.31em")
    .text((d) => getNodeHoverText(d.printNode))
    .clone(true)
    .lower()
    .attr("fill", "none")
    .attr("stroke", "white")
    .attr("stroke-width", 3);

  let transform: ZoomTransform = d3.zoomIdentity;
  let sqrtK = 1;
  const zoom = d3.zoom<SVGSVGElement, undefined>();
  zoom.on(
    "zoom",
    (e: D3ZoomEvent<SVGSVGElement, undefined>) => {
      transform = e.transform;
      nodeG.attr("transform", transform.toString());
      sqrtK = Math.sqrt(transform.k);
      nodeCircle.attr("r", nodeRadius / sqrtK).attr("stroke-width", 1 / sqrtK);

      linkG.attr("transform", transform.toString());
      link.style("stroke-width", linkThickness / sqrtK);

      arrow
        .attr("markerWidth", 5)
        .attr("markerHeight", 5)
        .attr("viewBox", `0 0 ${5 / sqrtK} ${5 / sqrtK}`)
        .attr("refX", 8 / sqrtK)
        .attr("refY", 2.5 / sqrtK);
      arrowInnerPath.attr(
        "d",
        `M 0 0 L ${5 / sqrtK} ${2.5 / sqrtK} L 0 ${5 / sqrtK} z`,
      );
      refreshSelectedNode();
    },
  );

  simulation.on("tick", () => {
    refreshLinks();

    let minX = Number.MAX_SAFE_INTEGER;
    let maxX = Number.MIN_SAFE_INTEGER;
    let minY = Number.MAX_SAFE_INTEGER;
    let maxY = Number.MIN_SAFE_INTEGER;
    nodeGInner.attr(
      "transform",
      (d) => {
        const x = d.x ?? 0;
        const y = d.y ?? 0;
        minX = Math.min(minX, x);
        maxX = Math.max(maxX, x);
        minY = Math.min(minY, y);
        maxY = Math.max(maxY, y);
        return `translate(${x}, ${y})`;
      },
    );

    if (!wasMouseActivity) {
      svg.call(
        zoom.transform,
        d3.zoomIdentity
          .translate(width / 2, height / 2)
          .scale(0.95 / Math.max((maxX - minX) / width, (maxY - minY) / height))
          .translate(-(minX + maxX) / 2, -(minY + maxY) / 2),
      );
    }
  });

  function refreshLinks() {
    link
      .attr("x1", (d) => endpoint(d.source).x ?? 0)
      .attr("y1", (d) => endpoint(d.source).y ?? 0)
      .attr("x2", (d) => endpoint(d.target).x ?? 0)
      .attr("y2", (d) => endpoint(d.target).y ?? 0);
  }

  let lastId = 0;
  const element = svg.call(zoom).call(zoom.transform, d3.zoomIdentity).node();
  if (element == null) {
    throw new Error("Failed to create the trace graph SVG.");
  }
  return {
    element,
    setSelectedNodeId(selectedNodeId: number) {
      d3.select(`#node${lastId}`)
        .attr("stroke", "#000")
        .attr("stroke-width", 1 / sqrtK)
        .attr("r", nodeRadius / sqrtK);
      d3.selectAll(`[data-originating-node-id="${lastId}"]`)
        .style("stroke-width", linkThickness / sqrtK)
        .attr("marker-end", "url(#end)");
      lastId = selectedNodeId;
      refreshSelectedNode();
    },
  };

  function refreshSelectedNode() {
    d3.select(`#node${lastId}`)
      .attr("stroke", "red")
      .attr("stroke-width", 4 / sqrtK)
      .attr("r", (nodeRadius + 10) / sqrtK);
    d3.selectAll(`[data-originating-node-id="${lastId}"]`)
      .style("stroke-width", (linkThickness + 15) / sqrtK)
      // not worth the hassle to resize this
      .attr("marker-end", "");
  }

  function getLineColor(d: GraphLink): string {
    return d.color || (d.originatingNodeId != null ? "blue" : "#000");
  }
}

function createNodeInfoArea() {
  const containerElement = document.createElement("div");
  containerElement.id = "node-info-area";
  const colorRectangle = document.createElement("span");
  colorRectangle.id = "color-rectangle";
  containerElement.appendChild(colorRectangle);
  const nameElement = document.createElement("span");
  containerElement.appendChild(nameElement);

  return {
    element: containerElement,
    setSelectedNodeId(selectedNodeId: number) {
      const printNode = traceResult.getPrintNode(selectedNodeId);
      colorRectangle.style.backgroundColor = getNodeColor(printNode);
      nameElement.textContent = getNodeHoverText(printNode);
    },
  };
}

function createInfoArea() {
  const mainElement = document.createElement("div");
  mainElement.id = "info-area";
  const currentTimeLabel = document.createElement("label");
  currentTimeLabel.textContent = "Time:";
  mainElement.appendChild(currentTimeLabel);
  const timeSpan = document.createElement("span");
  mainElement.appendChild(timeSpan);

  return {
    element: mainElement,
    setTraceIndex(index: number) {
      const trace = traceResult.traces[index];
      timeSpan.textContent = formatNanos(trace.nanos);
    },
  };
}

function createCodeView() {
  const tabChars = "→&nbsp;&nbsp;&nbsp;";
  const spaceChar = "·";
  const mainElement = document.createElement("div");
  mainElement.id = "code-view";
  let lastTraceIndex = -1;

  return {
    element: mainElement,
    setTraceIndex(index: number) {
      if (lastTraceIndex === index) {
        return;
      }

      const trace = traceResult.traces[index];
      clearElementChildren(mainElement);

      if (trace.writerNodeId != null) {
        const startWriterNode = traceResult.getWriterNode(trace.writerNodeId);

        const elements: HTMLElement[] = [];
        fillWriterNodeElements(startWriterNode, elements);
        elements.reverse(); // reverse to get them in forward order

        for (const childElement of elements) {
          mainElement.appendChild(childElement);
        }
      }

      // scroll to the bottom
      mainElement.scrollTop = mainElement.scrollHeight;

      lastTraceIndex = index;
    },
  };

  function fillWriterNodeElements(node: WriterNode, elements: HTMLElement[]) {
    if (node.text === "\n" || node.text === "\r\n") {
      elements.push(document.createElement("br"));
    } else {
      const texts = node.text.split(/\r?\n/);
      for (const [i, text] of texts.entries()) {
        if (i > 0) {
          elements.push(document.createElement("br"));
        }

        for (const segment of extractTextSegments(text).reverse()) {
          if (segment.kind === "text") {
            const element = document.createElement("span");
            element.className = "writer-node";
            element.innerText = segment.text;
            elements.push(element);
          } else if (segment.kind === "space") {
            const element = document.createElement("span");
            element.className = "writer-node writer-node-space";
            element.innerText = spaceChar.repeat(segment.count);
            elements.push(element);
          } else if (segment.kind === "tab") {
            const element = document.createElement("span");
            element.className = "writer-node writer-node-tab";
            element.innerHTML = tabChars.repeat(segment.count);
            elements.push(element);
          }
        }
      }
    }

    if (node.previousNodeId != null) {
      fillWriterNodeElements(
        traceResult.getWriterNode(node.previousNodeId),
        elements,
      );
    }
  }
}

function clearElementChildren(element: HTMLElement) {
  let last: ChildNode | null;
  while ((last = element.lastChild)) {
    element.removeChild(last);
  }
}

function createSlider(onChange: (value: number) => void) {
  const element = document.createElement("div");
  element.id = "slider";
  const input = document.createElement("input");
  input.type = "range";
  input.addEventListener("input", () => {
    // todo: debounce
    onChange(input.valueAsNumber);
  });
  input.min = "0";

  element.appendChild(input);

  return {
    element,
    setMax(max: number) {
      if (input.max !== max.toString()) {
        input.max = max.toString();
      }
    },
    setValue(value: number) {
      if (input.value !== value.toString()) {
        input.value = value.toString();
      }
    },
  };
}

function getTransformedTraceResult() {
  const writerNodes = getWriterNodesMap();
  const printNodes = getPrintNodesMap();
  return {
    traces: rawTraceResult.traces,
    printNodes: rawTraceResult.printNodes,
    getWriterNode(id: number): WriterNode {
      const node = writerNodes.get(id);
      if (node == null) {
        throw new Error(`Could not find writer node ${id}.`);
      }
      return node;
    },
    getPrintNode(id: number): PrintNode {
      const node = printNodes.get(id);
      if (node == null) {
        throw new Error(`Could not find print node ${id}.`);
      }
      return node;
    },
  };

  function getWriterNodesMap() {
    const map = new Map<number, WriterNode>();
    for (const node of rawTraceResult.writerNodes) {
      map.set(node.writerNodeId, node);
    }
    return map;
  }

  function getPrintNodesMap() {
    const map = new Map<number, PrintNode>();
    for (const node of rawTraceResult.printNodes) {
      map.set(node.printNodeId, node);
    }
    return map;
  }
}

function getNodeColor(node: PrintNode): string {
  switch (node.printItem.kind) {
    case "info":
      return "blue";
    case "condition":
      return "orange";
    case "signal":
      return "yellow";
    case "rcPath":
      return "green";
    case "string":
      return "#ccc";
    case "anchor":
      return "pink";
    case "conditionReevaluation":
      return "purple";
  }
}

function formatNanos(nanos: number): string {
  const characters = nanos.toString();
  let finalText = "";
  for (let i = 0; i < characters.length; i++) {
    if (i > 0 && i % 3 === 0) {
      finalText = `,${finalText}`;
    }
    finalText = characters[characters.length - 1 - i] + finalText;
  }
  return `${finalText}ns`;
}

function extractTextSegments(text: string): CodeViewTextSegment[] {
  let spaceCount = 0;
  let tabCount = 0;
  let lastIndex = 0;
  const segments: CodeViewTextSegment[] = [];

  for (let i = 0; i < text.length; i++) {
    const char = text[i];
    if (char === " ") {
      addTabIfNecessary();
      addLastTextIfNecessary(i);
      spaceCount++;
      lastIndex = i + 1;
    } else if (char === "\t") {
      addSpaceIfNecessary();
      addLastTextIfNecessary(i);
      tabCount++;
      lastIndex = i + 1;
    } else {
      addSpaceIfNecessary();
      addTabIfNecessary();
    }
  }

  addLastTextIfNecessary(text.length);
  addSpaceIfNecessary();
  addTabIfNecessary();

  return segments;

  function addTabIfNecessary() {
    if (tabCount > 0) {
      segments.push({
        kind: "tab",
        count: tabCount,
      });
      tabCount = 0;
    }
  }

  function addSpaceIfNecessary() {
    if (spaceCount > 0) {
      segments.push({
        kind: "space",
        count: spaceCount,
      });
      spaceCount = 0;
    }
  }

  function addLastTextIfNecessary(currentIndex: number) {
    if (lastIndex !== currentIndex) {
      segments.push({
        kind: "text",
        text: text.substring(lastIndex, currentIndex),
      });
    }
  }
}
