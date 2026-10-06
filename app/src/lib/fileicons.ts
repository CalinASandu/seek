import {
  AppWindow,
  File,
  FileArchive,
  FileAudio,
  FileCode,
  FileImage,
  FileSpreadsheet,
  FileText,
  FileVideo,
  Folder,
  Settings2,
  type LucideIcon,
} from "lucide-react";

export type IconGroup = "folder" | "doc" | "sheet" | "image" | "video" | "audio" | "archive" | "code" | "app" | "system" | "file";

const GROUPS: Record<string, IconGroup> = {};
const add = (g: IconGroup, exts: string) => exts.split(" ").forEach((e) => (GROUPS[e] = g));
add("doc", "pdf doc docx txt md rtf odt log");
add("sheet", "xls xlsx csv ods");
add("doc", "ppt pptx");
add("image", "jpg jpeg png gif bmp svg webp heic tif tiff ico psd");
add("video", "mp4 mkv avi mov wmv webm");
add("audio", "mp3 wav flac aac ogg m4a");
add("archive", "zip 7z rar tar gz iso");
add("code", "ts tsx js jsx rs py go java c cpp h cs json toml yaml yml html css xml sh ps1");
add("app", "exe msi bat cmd");
add("system", "dll sys ini cfg dat");

export function iconGroup(name: string, isDir: boolean): IconGroup {
  if (isDir) return "folder";
  const dot = name.lastIndexOf(".");
  if (dot < 0) return "file";
  return GROUPS[name.slice(dot + 1).toLowerCase()] ?? "file";
}

export const ICONS: Record<IconGroup, LucideIcon> = {
  folder: Folder,
  doc: FileText,
  sheet: FileSpreadsheet,
  image: FileImage,
  video: FileVideo,
  audio: FileAudio,
  archive: FileArchive,
  code: FileCode,
  app: AppWindow,
  system: Settings2,
  file: File,
};
