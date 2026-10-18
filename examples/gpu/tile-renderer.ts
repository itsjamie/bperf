// A deliberately naive WebGL tile renderer: one uniform update and one draw
// call per tile. Batching every tile into one buffer and one draw call is the
// optimization the example benchmark is meant to prove.

const VERTEX_SHADER = `
attribute vec2 position;
uniform vec2 offset;
uniform vec2 scale;
void main() {
  vec2 pixel = position * scale + offset;
  gl_Position = vec4(pixel / 128.0 - 1.0, 0.0, 1.0);
}`;

const FRAGMENT_SHADER = `
precision mediump float;
uniform vec4 color;
void main() {
  gl_FragColor = color;
}`;

export interface TileRenderer {
  readonly gl: WebGLRenderingContext;
  draw(): void;
}

function compile(gl: WebGLRenderingContext, type: number, source: string): WebGLShader {
  const shader = gl.createShader(type);
  if (!shader) throw new Error("could not create shader");
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    throw new Error(gl.getShaderInfoLog(shader) ?? "shader failed to compile");
  }
  return shader;
}

// Integer channel values keep the rendered bytes identical on every adapter.
function tileColor(column: number, row: number): [number, number, number] {
  const hash = Math.imul(column * 73_856_093 ^ row * 19_349_663, 2_654_435_761) >>> 0;
  return [hash & 0xff, (hash >>> 8) & 0xff, (hash >>> 16) & 0xff];
}

export function createTileRenderer(gl: WebGLRenderingContext, grid: number): TileRenderer {
  const program = gl.createProgram();
  if (!program) throw new Error("could not create program");
  gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, VERTEX_SHADER));
  gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, FRAGMENT_SHADER));
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    throw new Error(gl.getProgramInfoLog(program) ?? "program failed to link");
  }
  gl.useProgram(program);

  gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
  gl.bufferData(
    gl.ARRAY_BUFFER,
    new Float32Array([0, 0, 1, 0, 0, 1, 0, 1, 1, 0, 1, 1]),
    gl.STATIC_DRAW,
  );
  const position = gl.getAttribLocation(program, "position");
  gl.enableVertexAttribArray(position);
  gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);

  const offset = gl.getUniformLocation(program, "offset");
  const scale = gl.getUniformLocation(program, "scale");
  const color = gl.getUniformLocation(program, "color");
  const size = 256 / grid;
  gl.viewport(0, 0, 256, 256);
  gl.uniform2f(scale, size, size);

  return {
    gl,
    draw() {
      gl.clearColor(0, 0, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
      for (let row = 0; row < grid; row += 1) {
        for (let column = 0; column < grid; column += 1) {
          const [red, green, blue] = tileColor(column, row);
          gl.uniform2f(offset, column * size, row * size);
          gl.uniform4f(color, red / 255, green / 255, blue / 255, 1);
          gl.drawArrays(gl.TRIANGLES, 0, 6);
        }
      }
    },
  };
}
