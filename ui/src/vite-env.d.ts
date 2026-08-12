/// <reference types="vite/client" />

// Side-effect CSS imports have no type of their own; this tells the compiler they are legal
// rather than silencing the error at each import site.
declare module '*.css';
