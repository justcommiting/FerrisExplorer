// Archives that open as folders. Only the names decide this; the backend
// checks what the file really is when it is listed.
const ARCHIVE_NAME = /\.(zip|tar|tar\.gz|tgz|tar\.bz2|tar\.xz)$/i

export function isArchiveName(name: string): boolean {
  return ARCHIVE_NAME.test(name)
}
