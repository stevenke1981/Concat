// Writes the extension's icons (16, 48 and 128 px) as PNG files. Run once:
//   node tools/make-icons.mjs
// Needs only Node's built-in zlib, so no image library is installed. Each pixel is
// sampled 4x4 times so the edges come out smooth at small sizes.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { deflateSync } from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "..", "icons");

const BLUE = [37, 99, 235];
const WHITE = [255, 255, 255];
const SAMPLES = 4;

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(bytes) {
  let c = 0xffffffff;
  for (const byte of bytes) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const typeAndData = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(typeAndData));
  return Buffer.concat([length, typeAndData, crc]);
}

/** An 8-bit RGBA PNG from raw pixels. */
function encodePng(size, pixels) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8; // bit depth
  header[9] = 6; // colour type: RGBA
  const stride = size * 4;
  const raw = Buffer.alloc((stride + 1) * size); // each row starts with a filter byte (0 = none)
  for (let y = 0; y < size; y++) {
    pixels.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/**
 * A rounded blue square with a white "C", a clear mark at every size. Returns
 * RGBA pixels.
 */
function drawIcon(size) {
  const pixels = Buffer.alloc(size * size * 4);
  const half = size / 2;
  const corner = size * 0.22;
  const ringOuter = size * 0.36;
  const ringInner = size * 0.2;
  const openingDeg = 42; // the gap on the right of the C
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let blue = 0;
      let white = 0;
      for (let sy = 0; sy < SAMPLES; sy++) {
        for (let sx = 0; sx < SAMPLES; sx++) {
          const px = x + (sx + 0.5) / SAMPLES;
          const py = y + (sy + 0.5) / SAMPLES;
          const dx = Math.abs(px - half) - (half - corner);
          const dy = Math.abs(py - half) - (half - corner);
          const outside = Math.max(dx, 0) ** 2 + Math.max(dy, 0) ** 2 > corner ** 2;
          if (outside) continue;
          const rx = px - half;
          const ry = py - half;
          const radius = Math.hypot(rx, ry);
          const angle = Math.abs((Math.atan2(ry, rx) * 180) / Math.PI);
          const inRing = radius >= ringInner && radius <= ringOuter && angle > openingDeg;
          if (inRing) white++;
          else blue++;
        }
      }
      const total = SAMPLES * SAMPLES;
      const opaque = blue + white;
      const offset = (y * size + x) * 4;
      if (opaque === 0) continue; // fully transparent corner
      for (let channel = 0; channel < 3; channel++) {
        pixels[offset + channel] = Math.round(
          (white * WHITE[channel] + blue * BLUE[channel]) / opaque,
        );
      }
      pixels[offset + 3] = Math.round((opaque / total) * 255);
    }
  }
  return pixels;
}

mkdirSync(outDir, { recursive: true });
for (const size of [16, 48, 128]) {
  const file = join(outDir, `icon${size}.png`);
  writeFileSync(file, encodePng(size, drawIcon(size)));
  console.log(`wrote ${file}`);
}
