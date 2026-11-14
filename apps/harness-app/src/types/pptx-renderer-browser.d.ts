/**
 * @aiden0z/pptx-renderer/browser 的声明文件内部使用无扩展名相对导入：
 * packages/ui 自己的 NodeNext 工程会把它们解析成 any，而本应用的 Bundler 解析会拿到完整类型，
 * 从而暴露 @zcode/ui 内 pptxRendererPreviewEngine.ts 的既有类型不匹配。
 * @zcode/ui 不在本应用所有权内，这里按共享包自身实际获得的类型（any）补齐子路径声明，
 * 保证应用侧检查结果与共享包一致；待 ui 包修正该文件后删除本声明。
 */
declare module "@aiden0z/pptx-renderer/browser";
