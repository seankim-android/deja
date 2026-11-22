import { GlobalRegistrator } from '@happy-dom/global-registrator'

GlobalRegistrator.register({ url: 'http://127.0.0.1/', width: 1440, height: 1100 })
Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true })
