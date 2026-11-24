// The picture looks, each one WebGL2 pass, the same as the console's game host draws:
// - clean: sharp pixels, only the seam between two blended at sizes that are not a whole multiple
// - upscale: diagonal edges rebuilt from the four neighbours, the pixels kept sharp
// - crt: a beam blurred along the line, scanlines that widen with brightness, an aperture grille
// - lcd: a cell grid one screen pixel wide, with a little of a subpixel's warmth

export type ShaderMode = 'clean' | 'upscale' | 'crt' | 'lcd';
export const SHADERS: readonly ShaderMode[] = ['clean', 'upscale', 'crt', 'lcd'];

/** Whether a stored value is one of the looks. */
export const isShader = (value: unknown): value is ShaderMode => SHADERS.includes(value as ShaderMode);

const VERTEX = `#version 300 es
out vec2 uv;
void main() {
  // One triangle that covers the viewport; the picture's first row at the top.
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = vec2(p.x, 1.0 - p.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

const VERTEX_UPSCALE = `#version 300 es
uniform vec2 size;
out vec2 uv;
out vec4 v_ns;
out vec4 v_we;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  vec2 step = 1.0 / size;
  uv = vec2(p.x, 1.0 - p.y);
  v_ns = vec4(uv.x, uv.y - step.y, uv.x, uv.y + step.y);
  v_we = vec4(uv.x - step.x, uv.y, uv.x + step.x, uv.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

const FRAGMENT_CLEAN = `#version 300 es
precision highp float;
uniform sampler2D picture;
// The picture in pixels, and the screen's pixels to one of the picture's, each way.
uniform vec2 size;
uniform vec2 scale;
in vec2 uv;
out vec4 colour;
void main() {
  vec2 texel = uv * size;
  vec2 whole = max(floor(scale), vec2(1.0));
  vec2 inside = 0.5 - 0.5 / whole;
  vec2 offset = fract(texel) - 0.5;
  vec2 at = (offset - clamp(offset, -inside, inside)) * whole + 0.5;
  colour = texture(picture, (floor(texel) + at) / size);
}`;

const FRAGMENT_UPSCALE = `#version 300 es
precision highp float;
uniform sampler2D picture;
uniform vec2 size;
uniform vec2 scale;
in vec2 uv;
in vec4 v_ns;
in vec4 v_we;
out vec4 colour;
void main() {
  vec3 c = texture(picture, uv).rgb;
  vec3 n = texture(picture, v_ns.xy).rgb;
  vec3 s = texture(picture, v_ns.zw).rgb;
  vec3 w = texture(picture, v_we.xy).rgb;
  vec3 e = texture(picture, v_we.zw).rgb;
  vec2 off = fract(uv * size) - 0.5;
  vec3 v_nb = off.y < 0.0 ? n : s;
  vec3 h_nb = off.x < 0.0 ? w : e;
  const vec3 luma = vec3(0.299, 0.587, 0.114);
  float lc = dot(c, luma);
  float ln = dot(n, luma);
  float ls = dot(s, luma);
  float lw = dot(w, luma);
  float le = dot(e, luma);
  float lv = off.y < 0.0 ? ln : ls;
  float ov = abs(lc - (off.y < 0.0 ? ls : ln));
  float oh = abs(lc - (off.x < 0.0 ? le : lw));
  float opp = lv < lc ? min(ov, oh) : max(ov, oh);
  float d_diag = dot(abs(v_nb - h_nb), vec3(1.0));
  float d_cv   = dot(abs(c - v_nb), vec3(1.0));
  float is_corner = step(d_diag, 0.08) * step(0.08, d_cv) * step(opp, 0.04);
  float bevel = clamp((abs(off.x) + abs(off.y) - 0.5) * min(scale.x, scale.y) * 0.7 + 0.5, 0.0, 1.0);
  colour = vec4(mix(c, 0.5 * (v_nb + h_nb), is_corner * bevel), 1.0);
}`;

const FRAGMENT_CRT = `#version 300 es
precision highp float;
uniform sampler2D picture;
uniform vec2 size;
uniform vec2 scale;
in vec2 uv;
out vec4 colour;
void main() {
  vec2 texel = uv * size;
  vec2 at = floor(texel) + 0.5;
  vec2 off = texel - at;
  float inner = max(0.5 - 0.9 / scale.x, 0.0);
  float into = (off.x - clamp(off.x, -inner, inner)) / max(1.0 - 2.0 * inner, 0.001);
  vec3 col = texture(picture, vec2(at.x + into, at.y) / size).rgb;
  float luma = dot(col, vec3(0.299, 0.587, 0.114));
  float y = 2.0 * off.y;
  float beam = 1.0 - (y * y) * mix(0.65, 0.20, luma);
  float p = fract(gl_FragCoord.x * 0.5) - 0.5;
  vec3 mask = vec3(1.0 + p * 0.12, 1.0 - p * 0.08, 1.0 + p * 0.12);
  colour = vec4(clamp(col * beam * mask * 1.12, 0.0, 1.0), 1.0);
}`;

const FRAGMENT_LCD = `#version 300 es
precision highp float;
uniform sampler2D picture;
uniform vec2 size;
uniform vec2 scale;
in vec2 uv;
out vec4 colour;
void main() {
  vec2 texel = uv * size;
  vec2 at = floor(texel) + 0.5;
  vec2 off = texel - at;
  vec2 whole = max(floor(scale), vec2(1.0));
  vec2 inner = 0.5 - 0.5 / whole;
  vec2 into = (off - clamp(off, -inner, inner)) * whole;
  vec3 col = texture(picture, (at + into) / size).rgb;
  vec2 cell = clamp((0.5 - abs(off)) * scale - 0.25, 0.0, 1.0);
  float grid = mix(0.72, 1.0, cell.x * cell.y);
  vec3 sub = vec3(1.0 - 0.05 * off.x, 1.0, 1.0 + 0.05 * off.x);
  colour = vec4(col * grid * sub, 1.0);
}`;

export function shaderSources(mode: ShaderMode): { vs: string; fs: string; nearest: boolean } {
  switch (mode) {
    case 'upscale':
      return { vs: VERTEX_UPSCALE, fs: FRAGMENT_UPSCALE, nearest: true };
    case 'crt':
      return { vs: VERTEX, fs: FRAGMENT_CRT, nearest: false };
    case 'lcd':
      return { vs: VERTEX, fs: FRAGMENT_LCD, nearest: false };
    default:
      return { vs: VERTEX, fs: FRAGMENT_CLEAN, nearest: false };
  }
}
