const grouped = new Intl.NumberFormat(undefined, { maximumSignificantDigits: 21 });
const scientific = new Intl.NumberFormat(undefined, { notation: "scientific", maximumSignificantDigits: 21 });
/** Display only: preserve JS numeric precision; never round tiny values to zero. */
export function displayNumber(value: number): string {
  const magnitude = Math.abs(value);
  return magnitude !== 0 && (magnitude < 1e-6 || magnitude >= 1e21)
    ? scientific.format(value)
    : grouped.format(value);
}
