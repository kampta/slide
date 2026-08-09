export function parentPath(path: string): string | null {
  const trimmed = path.replace(/\/+$/, "");
  if (!trimmed || trimmed === "/") return null;
  const separator = trimmed.lastIndexOf("/");
  return separator <= 0 ? "/" : trimmed.slice(0, separator);
}
