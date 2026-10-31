declare const JSON5: {
  parse(text: string): unknown;
  stringify(value: unknown, space?: string | number): string;
};
export default JSON5;
