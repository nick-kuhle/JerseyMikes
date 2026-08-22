/** Creation bytecode imports (see next.config.mjs `asset/source` rule). */
declare module "*.hex" {
  const content: string;
  export default content;
}
