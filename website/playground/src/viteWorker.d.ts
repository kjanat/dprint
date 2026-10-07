// Matches the constructor Vite exports for a `?worker` import.
declare const workerConstructor: {
  new(options?: { name?: string }): Worker;
};

export default workerConstructor;
