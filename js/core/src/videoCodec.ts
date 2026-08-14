/**
 * AV1 `seq_level_idx` for a frame of this size at 60 fps, as the two-digit
 * string used by WebCodecs codec parameters. Mirrors the server's Table A.3
 * lookup so advertised configs and emitted bitstreams agree.
 */
export function av1LevelString(width: number, height: number): string {
  const pic = width * height;
  const rate = pic * 60;
  const specs: [number, number, number, number, number][] = [
    [0, 147456, 2048, 1152, 4423680],
    [1, 278784, 2816, 1584, 8363520],
    [4, 665856, 4352, 2448, 19975680],
    [5, 1065024, 5504, 3096, 31950720],
    [8, 2359296, 6144, 3456, 70778880],
    [9, 2359296, 6144, 3456, 141557760],
    [12, 8912896, 8192, 4352, 267386880],
    [13, 8912896, 8192, 4352, 534773760],
    [14, 8912896, 8192, 4352, 1069547520],
    [16, 35651584, 16384, 8704, 1069547520],
    [17, 35651584, 16384, 8704, 2139095040],
    [18, 35651584, 16384, 8704, 4278190080],
  ];
  for (const [idx, maxPic, maxW, maxH, maxRate] of specs) {
    if (pic <= maxPic && width <= maxW && height <= maxH && rate <= maxRate) {
      return String(idx).padStart(2, "0");
    }
  }
  return "19";
}
