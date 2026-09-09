export interface DiffLine { kind: "same" | "added" | "removed"; text: string; }
/** Exact line alignment for bounded previews; large inputs retain whole replacement. */
export function diffLines(before: string, after: string): DiffLine[] {
  const left = before === "" ? [] : before.split("\n");
  const right = after === "" ? [] : after.split("\n");
  if (left.length * right.length > 250_000) return [...left.map(text => ({kind:"removed" as const,text})),...right.map(text => ({kind:"added" as const,text}))];
  const width = right.length + 1;
  const lengths = new Uint32Array((left.length + 1) * width);
  for (let i=left.length-1;i>=0;i--) for(let j=right.length-1;j>=0;j--)
    lengths[i*width+j] = left[i] === right[j] ? lengths[(i+1)*width+j+1]!+1 : Math.max(lengths[(i+1)*width+j]!, lengths[i*width+j+1]!);
  const output: DiffLine[] = [];
  let i=0,j=0;
  while(i<left.length || j<right.length) {
    if(i<left.length && j<right.length && left[i] === right[j]) {output.push({kind:"same",text:left[i++]!});j++;}
    else if(j<right.length && (i===left.length || lengths[i*width+j+1]! > lengths[(i+1)*width+j]!)) output.push({kind:"added",text:right[j++]!});
    else output.push({kind:"removed",text:left[i++]!});
  }
  return output;
}
export function decodeText(bytes: Uint8Array): string | null {
  if(bytes.includes(0)) return null;
  try { return new TextDecoder("utf-8",{fatal:true}).decode(bytes); } catch { return null; }
}
