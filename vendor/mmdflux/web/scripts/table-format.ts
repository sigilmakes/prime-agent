type TableAlignment = "left" | "right";

export interface TableColumn<Row> {
  header: string;
  align?: TableAlignment;
  value: (row: Row) => string;
}

function pad(
  value: string,
  width: number,
  alignment: TableAlignment = "left",
): string {
  if (alignment === "right") {
    return value.padStart(width, " ");
  }

  return value.padEnd(width, " ");
}

function makeBorder(
  widths: number[],
  left: string,
  middle: string,
  right: string,
): string {
  return `${left}${widths.map((width) => "─".repeat(width + 2)).join(middle)}${right}`;
}

export function formatTable<Row>(
  rows: readonly Row[],
  columns: readonly TableColumn<Row>[],
): string {
  if (columns.length === 0) {
    return "";
  }

  const matrix = rows.map((row) => columns.map((column) => column.value(row)));
  const widths = columns.map((column, index) => {
    const rowMax = matrix.reduce((max, values) => {
      return Math.max(max, values[index].length);
    }, 0);
    return Math.max(column.header.length, rowMax);
  });

  const top = makeBorder(widths, "┌", "┬", "┐");
  const divider = makeBorder(widths, "├", "┼", "┤");
  const bottom = makeBorder(widths, "└", "┴", "┘");

  const headerRow = `│ ${columns
    .map((column, index) => {
      return pad(column.header, widths[index], column.align);
    })
    .join(" │ ")} │`;

  const bodyRows = matrix.map((values) => {
    const line = columns
      .map((column, index) => {
        return pad(values[index], widths[index], column.align);
      })
      .join(" │ ");
    return `│ ${line} │`;
  });

  return [top, headerRow, divider, ...bodyRows, bottom].join("\n");
}
