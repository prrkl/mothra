// Copyright 2020 Sly Gryphon
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

using System;
using System.Runtime.InteropServices;

namespace Example
{
    internal static class MothraInterop
    {
        // mothra.dll on Windows, libmothra.so on Linux, libmotha.dylib on OSX
        private const string DllName = "mothra";
        private const string IngressDllName = "mothra-ingress";
        
        [DllImport(DllName, EntryPoint = "libp2p_start", CallingConvention = CallingConvention.Cdecl)]
        public static extern unsafe void Start([In, Out] string[] args, int length);

        [DllImport(DllName, EntryPoint = "libp2p_send_gossip", CallingConvention = CallingConvention.Cdecl)]
        public static extern unsafe void SendGossip(sbyte* topicUtf8, int topicLength, sbyte* data, int dataLength);

        [DllImport(DllName, EntryPoint = "libp2p_send_rpc_request", CallingConvention = CallingConvention.Cdecl)]
        public static extern unsafe void SendRequest(sbyte* methodUtf8, int methodLength, sbyte* peerUtf8, int peerLength, sbyte* data, int dataLength);

        [DllImport(DllName, EntryPoint = "libp2p_send_rpc_response", CallingConvention = CallingConvention.Cdecl)]
        public static extern unsafe void SendResponse(sbyte* methodUtf8, int methodLength, sbyte* peerUtf8, int peerLength, sbyte* data, int dataLength);

        [DllImport(IngressDllName, EntryPoint = "libp2p_register_handlers", CallingConvention = CallingConvention.Cdecl)]
        public static extern unsafe void RegisterHandlers(DiscoveredPeer discoveredPeer, ReceiveGossip receiveGossip, ReceiveRpc receiveRpc);
        //public static extern unsafe void RegisterHandlers(IntPtr discoveredPeer, IntPtr receiveGossip, IntPtr receiveRpc);
        
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        public unsafe delegate void DiscoveredPeer(sbyte* peerUtf8, int peerLength);
        
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        public unsafe delegate void ReceiveGossip(sbyte* topicUtf8, int topicLength, sbyte* data, int dataLength);

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        public unsafe delegate void ReceiveRpc(sbyte* methodUtf8, int methodLength, int requestResponseFlag, sbyte* peerUtf8, int peerLength, sbyte* data, int dataLength);
    }
}