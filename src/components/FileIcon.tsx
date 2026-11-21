import {
  File,
  FileArchive,
  FileAudio,
  FileCode,
  FileCog,
  FileImage,
  FileJson,
  FileSpreadsheet,
  FileText,
  FileVideo,
  Folder,
  FolderSymlink,
  type LucideIcon,
} from 'lucide-react'

import type { VNode } from '@/lib/api'
import { extension } from '@/lib/format'
import { cn } from '@/lib/utils'

const BY_EXT: [string[], LucideIcon, string][] = [
  [['rs', 'py', 'js', 'ts', 'tsx', 'jsx', 'go', 'c', 'h', 'cpp', 'java', 'html', 'css', 'sh', 'ps1', 'bat'], FileCode, 'text-sky-300'],
  [['json', 'toml', 'yaml', 'yml', 'xml'], FileJson, 'text-amber-300'],
  [['md', 'txt', 'log', 'pdf', 'doc', 'docx', 'rtf'], FileText, 'text-zinc-300'],
  [['csv', 'xlsx', 'xls'], FileSpreadsheet, 'text-emerald-300'],
  [['png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'svg', 'ico'], FileImage, 'text-fuchsia-300'],
  [['mp4', 'mkv', 'mov', 'avi', 'webm'], FileVideo, 'text-rose-300'],
  [['mp3', 'wav', 'flac', 'ogg', 'm4a'], FileAudio, 'text-violet-300'],
  [['zip', 'tar', 'gz', 'rar', '7z'], FileArchive, 'text-orange-300'],
  [['exe', 'dll', 'msi', 'ini', 'cfg', 'lock'], FileCog, 'text-zinc-400'],
]

export function FileIcon({ node, className }: { node: VNode; className?: string }) {
  if (node.is_dir) {
    return <Folder className={cn('size-4 fill-folder/25 text-folder', className)} />
  }
  if (node.is_symlink) {
    return <FolderSymlink className={cn('size-4 text-folder', className)} />
  }
  const ext = extension(node.name)
  for (const [exts, Icon, color] of BY_EXT) {
    if (exts.includes(ext)) return <Icon className={cn('size-4', color, className)} />
  }
  return <File className={cn('size-4 text-zinc-400', className)} />
}
