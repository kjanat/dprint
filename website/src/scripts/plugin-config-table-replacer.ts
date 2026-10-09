import { getPluginSchemaUrl } from "./plugin-repository.ts";

interface SchemaOption {
  value: unknown;
  description: string | undefined;
}

interface SchemaDefinition {
  ref: string | undefined;
  description: string | undefined;
  type: string | undefined;
  default: unknown;
  oneOf: SchemaOption[] | undefined;
}

interface AstSpecificProperty {
  propertyName: string;
  definition: SchemaDefinition | null;
}

interface ConfigProperty extends SchemaDefinition {
  name: string;
  order: number;
  astSpecificProperties: AstSpecificProperty[];
}

interface ConfigTableItem {
  element: HTMLElement;
  url: string;
}

// generates the plugin config table
const replaceConfigTable = () => {
  for (const item of getPluginConfigTableItems()) {
    getDprintPluginConfig(item.url)
      .then((properties) => {
        const isOfficial = new URL(item.url).pathname.startsWith("/dprint/");
        const element = item.element;
        element.innerHTML = `<p>This information was auto generated from <a href="${item.url}">${item.url}</a>.</p>`;
        for (const property of properties) {
          const propertyContainer = document.createElement("div");
          element.appendChild(propertyContainer);
          try {
            // title
            const propertyTitle = document.createElement("h2");
            if (isOfficial && property.name === "preferSingleLine") {
              property.name += " (Very Experimental)";
            }
            propertyTitle.textContent = property.name;
            propertyContainer.appendChild(propertyTitle);

            addDescription(propertyContainer, property);
            addInfoContainer(propertyContainer, property);

            if (property.astSpecificProperties.length > 0) {
              const astSpecificPropertiesPrefix = document.createElement("p");
              astSpecificPropertiesPrefix.textContent = "AST node specific configuration property names:";
              propertyContainer.appendChild(astSpecificPropertiesPrefix);

              const astSpecificPropertyNamesContainer = document.createElement("ul");
              propertyContainer.appendChild(astSpecificPropertyNamesContainer);

              for (const { propertyName, definition } of property.astSpecificProperties) {
                const propertyNameLi = document.createElement("li");

                const labelSpan = document.createElement("span");
                labelSpan.textContent = valueToText(propertyName);
                propertyNameLi.appendChild(labelSpan);

                if (definition != null) {
                  const definitionDiv = document.createElement("div");
                  if (definition.description !== property.description) {
                    addDescription(definitionDiv, definition);
                  }
                  addInfoContainer(definitionDiv, definition);
                  propertyNameLi.appendChild(definitionDiv);
                }

                astSpecificPropertyNamesContainer.appendChild(propertyNameLi);
              }
            }
          } catch (err) {
            console.error(err);
            const errorMessage = document.createElement("strong");
            errorMessage.textContent = "Error getting property information. Check the browser console.";
            errorMessage.style.color = "red";
            propertyContainer.appendChild(errorMessage);
          }
        }
      })
      .catch((err) => {
        console.error("Error loading plugin configuration.", err);
        item.element.textContent = "Unable to load configuration information. Please try again later.";
      });
  }
};

function addDescription(propertyContainer: HTMLElement, definition: SchemaDefinition) {
  const propertyDesc = document.createElement("p");
  propertyDesc.textContent = definition.description ?? "";
  propertyContainer.appendChild(propertyDesc);
}

function addInfoContainer(propertyContainer: HTMLElement, definition: SchemaDefinition) {
  const infoContainer = document.createElement("ul");
  propertyContainer.appendChild(infoContainer);

  if (definition.oneOf) {
    for (const oneOf of definition.oneOf) {
      const oneOfContainer = document.createElement("li");
      infoContainer.appendChild(oneOfContainer);
      const prefix = document.createElement("strong");
      prefix.textContent = valueToText(oneOf.value);
      oneOfContainer.appendChild(prefix);
      if (oneOf.description != null && oneOf.description.length > 0) {
        oneOfContainer.append(` - ${oneOf.description}`);
      }
      if (oneOf.value === definition.default) {
        oneOfContainer.append(" (Default)");
      }
    }
  } else {
    // type
    const typeContainer = document.createElement("li");
    infoContainer.appendChild(typeContainer);
    const typePrefix = document.createElement("strong");
    typePrefix.textContent = "Type: ";
    typeContainer.appendChild(typePrefix);
    typeContainer.append(definition.type ?? "");

    // default
    const defaultContainer = document.createElement("li");
    infoContainer.appendChild(defaultContainer);
    const defaultPrefix = document.createElement("strong");
    defaultPrefix.textContent = "Default: ";
    defaultContainer.appendChild(defaultPrefix);
    defaultContainer.append(valueToText(definition.default));
  }
}

function valueToText(value: unknown): string {
  if (value === undefined) {
    return "<not specified>";
  }
  return JSON.stringify(value);
}

function getPluginConfigTableItems(): ConfigTableItem[] {
  const result: ConfigTableItem[] = [];
  for (const element of document.querySelectorAll<HTMLElement>(".plugin-config-table")) {
    const { url } = element.dataset;
    if (url != null) result.push({ element, url });
  }
  return result;
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error(`${path} is not an object`);
  return Object.fromEntries(Object.entries(value));
}

function toDefinition(value: unknown, path: string): SchemaDefinition {
  const source = record(value, path);
  return {
    ref: typeof source.$ref === "string" ? source.$ref : undefined,
    description: typeof source.description === "string" ? source.description : undefined,
    type: typeof source.type === "string" ? source.type : undefined,
    default: source.default,
    oneOf: Array.isArray(source.oneOf)
      ? source.oneOf.map((entry, index) => {
        const option = record(entry, `${path}.oneOf[${index}]`);
        return { value: option.const, description: typeof option.description === "string" ? option.description : undefined };
      })
      : undefined,
  };
}

const getDprintPluginConfig = async (configSchemaUrl: string): Promise<ConfigProperty[]> => {
  const response = await fetch(await getPluginSchemaUrl(configSchemaUrl));
  if (!response.ok) {
    throw new Error(
      `Error fetching plugin configuration: HTTP ${response.status}`,
    );
  }
  const json = record(await response.json(), "schema");
  const schemaProperties = record(json.properties, "schema.properties");
  const definitions = record(json.definitions ?? {}, "schema.definitions");
  const properties = new Map<string, ConfigProperty>();
  let order = 0;

  const ensure = (propertyName: string): ConfigProperty => {
    const existing = properties.get(propertyName);
    if (existing != null) return existing;
    const created: ConfigProperty = {
      name: propertyName,
      order: -1,
      astSpecificProperties: [],
      ref: undefined,
      description: undefined,
      type: undefined,
      default: undefined,
      oneOf: undefined,
    };
    properties.set(propertyName, created);
    return created;
  };
  const assign = (propertyName: string, definition: SchemaDefinition) => {
    const property = ensure(propertyName);
    Object.assign(property, definition);
    property.order = order++;
    property.name = propertyName;
  };

  for (const propertyName of Object.keys(schemaProperties)) {
    if (
      propertyName === "$schema"
      || propertyName === "deno"
      || propertyName === "locked"
    ) {
      continue;
    }
    const property = toDefinition(schemaProperties[propertyName], `schema.properties.${propertyName}`);

    if (property.ref != null) {
      const derivedPropName = property.ref.replace("#/definitions/", "");

      const lastSegment = propertyName.split(".").pop() ?? propertyName;
      let parentProperty: string | undefined;
      if (
        derivedPropName !== propertyName
        && derivedPropName in schemaProperties
      ) {
        parentProperty = derivedPropName;
      } else if (lastSegment !== propertyName && lastSegment in schemaProperties) {
        parentProperty = lastSegment;
      }

      const definition = toDefinition(definitions[derivedPropName], `schema.definitions.${derivedPropName}`);
      if (parentProperty != null) {
        const parentRef = toDefinition(schemaProperties[parentProperty], `schema.properties.${parentProperty}`).ref;
        const isSameDefinition = property.ref === parentRef;
        ensure(parentProperty).astSpecificProperties.push({
          propertyName,
          definition: isSameDefinition ? null : definition,
        });
      } else assign(propertyName, definition);
    } else {
      assign(propertyName, property);
    }
  }
  return [...properties.values()].sort((a, b) => a.order - b.order);
};

export { replaceConfigTable };
