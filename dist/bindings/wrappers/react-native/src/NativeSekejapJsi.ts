// TurboModule spec (New Architecture). Its sole job is to install the sync JSI
// HostObject (`global.SekejapJSI`) at startup; all real calls go through JSI, not
// the module methods. RN codegen turns this into the native interface.
import type { TurboModule } from 'react-native';
import { TurboModuleRegistry } from 'react-native';

export interface Spec extends TurboModule {
  // Installs global.SekejapJSI by calling the C++ `sekejap::install(runtime)`.
  install(): boolean;
}

export default TurboModuleRegistry.getEnforcing<Spec>('SekejapJsi');
