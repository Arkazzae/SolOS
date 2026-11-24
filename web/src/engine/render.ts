// Draws the console's picture on the page's canvas, from the worker: WebGL2, the frame as a texture,
// at the largest size that fits with the console's pixel shape (the NES's 8:7), centred on black.

import { type ShaderMode, shaderSources } from './shaders.ts';

export class Screen {
  private readonly gl: WebGL2RenderingContext;
  private program: WebGLProgram;
  private size: WebGLUniformLocation | null = null;
  private scale: WebGLUniformLocation | null = null;
  private readonly canvas: OffscreenCanvas;
  private shaderMode: ShaderMode;
  private width = 0;
  private height = 0;
  private aspect = 1;

  constructor(canvas: OffscreenCanvas, shader: ShaderMode = 'clean') {
    this.canvas = canvas;
    this.shaderMode = shader;
    const gl = canvas.getContext('webgl2', { alpha: false, antialias: false, depth: false, desynchronized: true });
    if (!gl) throw new Error('WebGL2 is not available');
    this.gl = gl;
    gl.bindVertexArray(gl.createVertexArray());
    gl.bindTexture(gl.TEXTURE_2D, gl.createTexture());
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.clearColor(0, 0, 0, 1);
    this.program = this.buildProgram(shader);
  }

  private buildProgram(mode: ShaderMode): WebGLProgram {
    const gl = this.gl;
    const { vs: vsSource, fs: fsSource, nearest } = shaderSources(mode);
    const program = gl.createProgram()!;
    for (const [type, source] of [
      [gl.VERTEX_SHADER, vsSource],
      [gl.FRAGMENT_SHADER, fsSource],
    ] as const) {
      const shader = gl.createShader(type)!;
      gl.shaderSource(shader, source);
      gl.compileShader(shader);
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader) ?? 'shader');
      gl.attachShader(program, shader);
    }
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program) ?? 'program');
    gl.useProgram(program);
    this.size = gl.getUniformLocation(program, 'size');
    this.scale = gl.getUniformLocation(program, 'scale');
    const filter = nearest ? gl.NEAREST : gl.LINEAR;
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
    return program;
  }

  setShader(mode: ShaderMode): void {
    if (this.shaderMode === mode) return;
    this.shaderMode = mode;
    const prev = this.program;
    this.program = this.buildProgram(mode);
    this.gl.deleteProgram(prev);
    this.present();
  }

  /** The canvas's size in the screen's own pixels. */
  resize(width: number, height: number): void {
    this.canvas.width = Math.max(1, width);
    this.canvas.height = Math.max(1, height);
    this.present();
  }

  /** Draws a frame: RGBA, width times height, its pixels pixelAspect wide to 1 tall. */
  draw(pixels: Uint8ClampedArray, width: number, height: number, pixelAspect: number): void {
    const gl = this.gl;
    if (width !== this.width || height !== this.height) {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, width, height, 0, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
      this.width = width;
      this.height = height;
    } else {
      gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, width, height, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
    }
    this.aspect = pixelAspect;
    this.present();
  }

  /** Draws the last frame again, as after a resize. */
  present(): void {
    const gl = this.gl;
    const { width: cw, height: ch } = this.canvas;
    gl.viewport(0, 0, cw, ch);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (this.width === 0) return;
    // The picture's shape on the screen, fitted whole and centred, on whole pixels.
    const shown = (this.width * this.aspect) / this.height;
    const w = Math.min(cw, Math.round(ch * shown));
    const h = Math.min(ch, Math.round(cw / shown));
    gl.viewport(Math.floor((cw - w) / 2), Math.floor((ch - h) / 2), w, h);
    gl.uniform2f(this.size, this.width, this.height);
    gl.uniform2f(this.scale, w / this.width, h / this.height);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }
}
