.globl CreateDirect3D11DeviceFromDXGIDevice
CreateDirect3D11DeviceFromDXGIDevice:
    jmp qword ptr [rip + HQX_TABLE + 0]
.globl CreateDirect3D11SurfaceFromDXGISurface
CreateDirect3D11SurfaceFromDXGISurface:
    jmp qword ptr [rip + HQX_TABLE + 8]
.globl D3D11CoreCreateDevice
D3D11CoreCreateDevice:
    jmp qword ptr [rip + HQX_TABLE + 16]
.globl D3D11CoreCreateLayeredDevice
D3D11CoreCreateLayeredDevice:
    jmp qword ptr [rip + HQX_TABLE + 24]
.globl D3D11CoreGetLayeredDeviceSize
D3D11CoreGetLayeredDeviceSize:
    jmp qword ptr [rip + HQX_TABLE + 32]
.globl D3D11CoreRegisterLayers
D3D11CoreRegisterLayers:
    jmp qword ptr [rip + HQX_TABLE + 40]
.globl D3D11CreateDeviceForD3D12
D3D11CreateDeviceForD3D12:
    jmp qword ptr [rip + HQX_TABLE + 48]
.globl D3D11On12CreateDevice
D3D11On12CreateDevice:
    jmp qword ptr [rip + HQX_TABLE + 56]
.globl D3DKMTCloseAdapter
D3DKMTCloseAdapter:
    jmp qword ptr [rip + HQX_TABLE + 64]
.globl D3DKMTCreateAllocation
D3DKMTCreateAllocation:
    jmp qword ptr [rip + HQX_TABLE + 72]
.globl D3DKMTCreateContext
D3DKMTCreateContext:
    jmp qword ptr [rip + HQX_TABLE + 80]
.globl D3DKMTCreateDevice
D3DKMTCreateDevice:
    jmp qword ptr [rip + HQX_TABLE + 88]
.globl D3DKMTCreateSynchronizationObject
D3DKMTCreateSynchronizationObject:
    jmp qword ptr [rip + HQX_TABLE + 96]
.globl D3DKMTDestroyAllocation
D3DKMTDestroyAllocation:
    jmp qword ptr [rip + HQX_TABLE + 104]
.globl D3DKMTDestroyContext
D3DKMTDestroyContext:
    jmp qword ptr [rip + HQX_TABLE + 112]
.globl D3DKMTDestroyDevice
D3DKMTDestroyDevice:
    jmp qword ptr [rip + HQX_TABLE + 120]
.globl D3DKMTDestroySynchronizationObject
D3DKMTDestroySynchronizationObject:
    jmp qword ptr [rip + HQX_TABLE + 128]
.globl D3DKMTEscape
D3DKMTEscape:
    jmp qword ptr [rip + HQX_TABLE + 136]
.globl D3DKMTGetContextSchedulingPriority
D3DKMTGetContextSchedulingPriority:
    jmp qword ptr [rip + HQX_TABLE + 144]
.globl D3DKMTGetDeviceState
D3DKMTGetDeviceState:
    jmp qword ptr [rip + HQX_TABLE + 152]
.globl D3DKMTGetDisplayModeList
D3DKMTGetDisplayModeList:
    jmp qword ptr [rip + HQX_TABLE + 160]
.globl D3DKMTGetMultisampleMethodList
D3DKMTGetMultisampleMethodList:
    jmp qword ptr [rip + HQX_TABLE + 168]
.globl D3DKMTGetRuntimeData
D3DKMTGetRuntimeData:
    jmp qword ptr [rip + HQX_TABLE + 176]
.globl D3DKMTGetSharedPrimaryHandle
D3DKMTGetSharedPrimaryHandle:
    jmp qword ptr [rip + HQX_TABLE + 184]
.globl D3DKMTLock
D3DKMTLock:
    jmp qword ptr [rip + HQX_TABLE + 192]
.globl D3DKMTOpenAdapterFromHdc
D3DKMTOpenAdapterFromHdc:
    jmp qword ptr [rip + HQX_TABLE + 200]
.globl D3DKMTOpenResource
D3DKMTOpenResource:
    jmp qword ptr [rip + HQX_TABLE + 208]
.globl D3DKMTPresent
D3DKMTPresent:
    jmp qword ptr [rip + HQX_TABLE + 216]
.globl D3DKMTQueryAdapterInfo
D3DKMTQueryAdapterInfo:
    jmp qword ptr [rip + HQX_TABLE + 224]
.globl D3DKMTQueryAllocationResidency
D3DKMTQueryAllocationResidency:
    jmp qword ptr [rip + HQX_TABLE + 232]
.globl D3DKMTQueryResourceInfo
D3DKMTQueryResourceInfo:
    jmp qword ptr [rip + HQX_TABLE + 240]
.globl D3DKMTRender
D3DKMTRender:
    jmp qword ptr [rip + HQX_TABLE + 248]
.globl D3DKMTSetAllocationPriority
D3DKMTSetAllocationPriority:
    jmp qword ptr [rip + HQX_TABLE + 256]
.globl D3DKMTSetContextSchedulingPriority
D3DKMTSetContextSchedulingPriority:
    jmp qword ptr [rip + HQX_TABLE + 264]
.globl D3DKMTSetDisplayMode
D3DKMTSetDisplayMode:
    jmp qword ptr [rip + HQX_TABLE + 272]
.globl D3DKMTSetDisplayPrivateDriverFormat
D3DKMTSetDisplayPrivateDriverFormat:
    jmp qword ptr [rip + HQX_TABLE + 280]
.globl D3DKMTSetGammaRamp
D3DKMTSetGammaRamp:
    jmp qword ptr [rip + HQX_TABLE + 288]
.globl D3DKMTSetVidPnSourceOwner
D3DKMTSetVidPnSourceOwner:
    jmp qword ptr [rip + HQX_TABLE + 296]
.globl D3DKMTSignalSynchronizationObject
D3DKMTSignalSynchronizationObject:
    jmp qword ptr [rip + HQX_TABLE + 304]
.globl D3DKMTUnlock
D3DKMTUnlock:
    jmp qword ptr [rip + HQX_TABLE + 312]
.globl D3DKMTWaitForSynchronizationObject
D3DKMTWaitForSynchronizationObject:
    jmp qword ptr [rip + HQX_TABLE + 320]
.globl D3DKMTWaitForVerticalBlankEvent
D3DKMTWaitForVerticalBlankEvent:
    jmp qword ptr [rip + HQX_TABLE + 328]
.globl D3DPerformance_BeginEvent
D3DPerformance_BeginEvent:
    jmp qword ptr [rip + HQX_TABLE + 336]
.globl D3DPerformance_EndEvent
D3DPerformance_EndEvent:
    jmp qword ptr [rip + HQX_TABLE + 344]
.globl D3DPerformance_GetStatus
D3DPerformance_GetStatus:
    jmp qword ptr [rip + HQX_TABLE + 352]
.globl D3DPerformance_SetMarker
D3DPerformance_SetMarker:
    jmp qword ptr [rip + HQX_TABLE + 360]
.globl EnableFeatureLevelUpgrade
EnableFeatureLevelUpgrade:
    jmp qword ptr [rip + HQX_TABLE + 368]
.globl OpenAdapter10
OpenAdapter10:
    jmp qword ptr [rip + HQX_TABLE + 376]
.globl OpenAdapter10_2
OpenAdapter10_2:
    jmp qword ptr [rip + HQX_TABLE + 384]
